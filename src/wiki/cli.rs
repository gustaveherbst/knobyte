//! `knobyte wiki ...` and `knobyte export` command definitions and handlers.

use std::fs;

use clap::Subcommand;
use colored::Colorize;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::wiki::diagnostics::DiagExt;
use crate::wiki::index::{EntitySummary, QueryFilter, WikiIndex};
use crate::wiki::models::WikiDiagnostic;
use crate::wiki::ops::{apply_operations, load_operation_file, ApplyOptions, OpActor};
use crate::wiki::scope::WikiScope;
use crate::wiki::synthesis::{self, SynthesisEnv};

type CliResult = Result<(), Box<dyn std::error::Error>>;

#[derive(clap::Args, Debug, Clone, Default)]
pub struct FilterArgs {
    /// Only these entity types (repeatable or comma-separated)
    #[arg(long = "type")]
    pub types: Vec<String>,
    /// Only members of this topic (id, title or alias)
    #[arg(long)]
    pub topic: Option<String>,
    /// Only these lifecycle states: in_flight, promoted, deprecated, archived
    #[arg(long)]
    pub status: Vec<String>,
    /// Only these grounding health values: fresh, unverified, ambiguous, changed, missing, none
    #[arg(long)]
    pub health: Vec<String>,
    /// Maximum results (default 50, max 500)
    #[arg(long)]
    pub limit: Option<usize>,
    /// Include archived entities (hidden by default)
    #[arg(long = "include-archived")]
    pub include_archived: bool,
    /// Skip this many results (paging; continue while `truncated`)
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
}

fn split_list(v: &[String]) -> Vec<String> {
    v.iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl FilterArgs {
    fn to_filter(&self) -> QueryFilter {
        QueryFilter {
            types: split_list(&self.types),
            topic: self.topic.clone(),
            statuses: split_list(&self.status),
            health: split_list(&self.health),
            include_archived: self.include_archived,
            file: None,
            limit: self.limit,
            offset: self.offset,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum WikiCommands {
    /// List entities (archived hidden unless --include-archived)
    List {
        #[command(flatten)]
        filter: FilterArgs,
        #[arg(long)]
        json: bool,
        /// One JSON object per line
        #[arg(long)]
        jsonl: bool,
    },
    /// Show one entity with relations, backlinks, groundings and sources
    Show {
        id: String,
        /// Omit the body
        #[arg(long = "no-body")]
        no_body: bool,
        /// Page size for relations and backlinks (default 25, max 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Skip this many relations and backlinks (use the reported nextOffset)
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Ranked search: id > title > summary > body
    Query {
        text: Vec<String>,
        #[command(flatten)]
        filter: FilterArgs,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jsonl: bool,
    },
    /// Bounded neighbourhood of an entity
    Related {
        id: String,
        /// Traversal depth (default 2, max 5)
        #[arg(long)]
        depth: Option<usize>,
        /// Token budget for the reached entities (default 4000)
        #[arg(long = "max-tokens")]
        max_tokens: Option<usize>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long = "include-archived")]
        include_archived: bool,
        #[arg(long)]
        json: bool,
    },
    /// Entities that relate to this one
    Backlinks {
        id: String,
        /// Page size (default 25, max 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Skip this many backlinks (use the reported nextOffset)
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Entities grounded in code symbols (graph ids or readable refs)
    ForCode {
        #[arg(required = true)]
        node_ids: Vec<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Include archived entities (hidden by default)
        #[arg(long = "include-archived")]
        include_archived: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jsonl: bool,
    },
    /// Bounded slice of the entity graph (around the given ids, or the first --limit entities)
    Graph {
        ids: Vec<String>,
        #[arg(long)]
        depth: Option<usize>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long = "include-archived")]
        include_archived: bool,
        #[arg(long)]
        json: bool,
    },
    /// Spec → requirement → decision → component → code → test traceability
    Trace {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Validate the wiki Markdown (no index needed)
    Validate {
        #[arg(long)]
        json: bool,
        /// Maximum diagnostics shown
        #[arg(long)]
        limit: Option<usize>,
        /// Accepted for compatibility: validate always exits non-zero on error-severity findings
        #[arg(long)]
        strict: bool,
    },
    /// Rebuild the wiki search index (and sync CozoDB)
    RebuildIndex {
        /// Only re-read files whose content hash changed
        #[arg(long)]
        incremental: bool,
        #[arg(long)]
        json: bool,
    },
    /// Apply typed wiki operations from a JSON file (object, array, or JSONL)
    Apply {
        file: String,
        /// Plan and print the changes without writing anything
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rewrite stale `<!-- kb:generated:begin -->` sections
    RegenerateViews {
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Agent-driven synthesis of wiki knowledge from the code graph
    Synthesis {
        #[command(subcommand)]
        sub: SynthesisCommands,
    },
    /// Rewrite older Knobyte wiki formats (legacy statuses, `document` types, derived ids,
    /// old grounding shapes, legacy `edges`) through audited operations. Plans only unless --apply.
    Migrate {
        /// Plan and report without writing (the default)
        #[arg(long = "dry-run", conflicts_with = "apply")]
        dry_run: bool,
        /// Apply the planned migration
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },
    /// Index maintenance: state, normalized dump, doctor
    Index {
        #[command(subcommand)]
        sub: IndexCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum IndexCommands {
    /// The index state (missing, fresh, stale, degraded, rebuild_required, corrupt,
    /// migration_required) with its revision
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Deterministic normalized dump of the index (rows ordered by key, wall-clock excluded)
    Dump {
        /// Write the dump to a file instead of stdout
        #[arg(long)]
        out: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Integrity check plus a diff of the index against a clean rebuild of the Markdown
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

impl WikiCommands {
    /// Whether the command was asked for `--json` (errors are then reported as an envelope).
    pub fn wants_json(&self) -> bool {
        match self {
            WikiCommands::List { json, .. }
            | WikiCommands::Show { json, .. }
            | WikiCommands::Query { json, .. }
            | WikiCommands::Related { json, .. }
            | WikiCommands::Backlinks { json, .. }
            | WikiCommands::ForCode { json, .. }
            | WikiCommands::Graph { json, .. }
            | WikiCommands::Trace { json, .. }
            | WikiCommands::Validate { json, .. }
            | WikiCommands::RebuildIndex { json, .. }
            | WikiCommands::Apply { json, .. }
            | WikiCommands::RegenerateViews { json, .. }
            | WikiCommands::Migrate { json, .. } => *json,
            WikiCommands::Index { sub } => match sub {
                IndexCommands::Status { json }
                | IndexCommands::Dump { json, .. }
                | IndexCommands::Doctor { json } => *json,
            },
            WikiCommands::Synthesis { sub } => match sub {
                SynthesisCommands::Build { json, .. }
                | SynthesisCommands::Prepare { json, .. }
                | SynthesisCommands::Propose { json, .. } => *json,
            },
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum SynthesisCommands {
    /// Discover clusters and produce the agent playbook
    Build {
        #[arg(long)]
        cluster: Option<String>,
        /// Print the playbook instead of saving it under .knobyte/local/synthesis/
        #[arg(long)]
        print: bool,
        #[arg(long)]
        json: bool,
    },
    /// Deterministic context and prompts for one stage
    Prepare {
        /// architecture_component | pattern | convention | global | relationships
        #[arg(long)]
        stage: String,
        #[arg(long)]
        cluster: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Validate an agent response into operation plans (writes only with --apply)
    Propose {
        file: String,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        stage: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn print_jsonl<T: Serialize>(items: &[T]) -> CliResult {
    for i in items {
        println!("{}", serde_json::to_string(i)?);
    }
    Ok(())
}

fn summary_line(s: &EntitySummary) -> String {
    let health = s
        .health
        .as_deref()
        .map(|h| format!(" [{}]", h))
        .unwrap_or_default();
    let status = if s.status == "promoted" {
        String::new()
    } else {
        format!(" ({})", s.status)
    };
    format!(
        "[{}] {} ({}){}{} {}:{}",
        s.entity_type.cyan(),
        s.title.bold(),
        s.id,
        status,
        health,
        s.file,
        s.start_line
    )
}

fn print_diags(diags: &[WikiDiagnostic]) {
    for d in diags {
        let sev = match d.severity.as_str() {
            "error" => d.severity.red().bold(),
            "warning" => d.severity.yellow(),
            _ => d.severity.dimmed(),
        };
        let loc = match d.line {
            Some(l) if !d.file.is_empty() => format!(" ({}:{})", d.file, l),
            None if !d.file.is_empty() => format!(" ({})", d.file),
            _ => String::new(),
        };
        println!("{} [{}] {}{}", sev, d.code.yellow(), d.message, loc);
    }
}

fn default_actor(config: &KnobyteConfig) -> OpActor {
    match crate::team::members::get_current_member(config) {
        Some(m) => OpActor {
            kind: "human".into(),
            id: m.id,
            session_id: None,
        },
        None => OpActor {
            kind: "human".into(),
            id: "cli".into(),
            session_id: None,
        },
    }
}

pub use crate::wiki::envelope::{exit_code, exit_code_for};
use crate::wiki::envelope::{envelope_for, failure};

/// Print `data` and `diagnostics` as the wiki envelope; exit with the envelope's status when
/// it is not 0.
pub fn emit<T: Serialize>(data: &T, diagnostics: &[WikiDiagnostic]) -> CliResult {
    let env = envelope_for(data, diagnostics);
    println!("{}", serde_json::to_string_pretty(&env)?);
    let code = env.exit_code();
    if code != exit_code::OK {
        std::process::exit(code);
    }
    Ok(())
}

/// Print a failure envelope (or the plain message) and exit with its status.
fn fail(json: bool, diagnostics: &[WikiDiagnostic]) -> ! {
    let env = failure(diagnostics);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&env).unwrap_or_default()
        );
    } else {
        for d in diagnostics {
            eprintln!("{} [{}] {}", "[error]".red().bold(), d.code, d.message);
        }
    }
    std::process::exit(env.exit_code())
}

fn not_found(json: bool, id: &str) -> ! {
    fail(
        json,
        &[crate::wiki::diagnostics::diag(
            "ENTITY_NOT_FOUND",
            format!("Entity '{}' not found", id),
            "",
        )
        .for_entity(id)],
    )
}

/// Open the index and bring it up to date with the Markdown (incremental). An index built by
/// another schema version is not touched: this fails with `WIKI_INDEX_REBUILD_REQUIRED`. A
/// refresh that stops at a corpus bound keeps the existing rows and warns.
pub fn open_fresh_index(config: &KnobyteConfig) -> Result<WikiIndex, Box<dyn std::error::Error>> {
    Ok(open_refreshed_index(config)?.0)
}

/// [`open_fresh_index`] that also returns the refresh failure as a diagnostic, so `--json`
/// envelopes carry it (and are not `ok`) instead of only printing it to stderr.
///
/// A corpus bound (`WIKI_CORPUS_LIMIT_EXCEEDED`) is an error: the answer omits Markdown the
/// index could not reach. `WIKI_INDEX_BUSY` is a warning: the answer comes from the last
/// published index generation, which is consistent but may be behind the Markdown.
pub fn open_refreshed_index(
    config: &KnobyteConfig,
) -> Result<(WikiIndex, Vec<WikiDiagnostic>), Box<dyn std::error::Error>> {
    let mut index = WikiIndex::open(&config.wiki_db_path())?;
    let mut diags = Vec::new();
    if let Err(e) = index.refresh(&config.scaffold_root) {
        eprintln!("{} wiki index refresh failed: {}", "[warn]".yellow(), e);
        diags.push(refresh_failure_diagnostic(&e));
    }
    Ok((index, diags))
}

/// The diagnostic a failed refresh is reported as (shared by the CLI and MCP adapters).
pub fn refresh_failure_diagnostic(e: &rusqlite::Error) -> WikiDiagnostic {
    let code = crate::wiki::index::error_code(e).unwrap_or("INDEX_REFRESH_REQUIRED");
    let message = e.to_string();
    let message = message
        .strip_prefix(&format!("{}: ", code))
        .unwrap_or(&message)
        .to_string();
    let mut d = crate::wiki::diagnostics::diag(
        code,
        format!("The index was not refreshed: {}", message),
        "",
    );
    match code {
        "WIKI_INDEX_BUSY" | "INDEX_REFRESH_REQUIRED" | "OPERATION_INTERRUPTED" => {
            d.severity = "warning".into();
            d.message.push_str(" Results come from the last published index.");
        }
        _ => d.severity = "error".into(),
    }
    d
}

/// Print a list result with its `truncated` flag: `--json` is `{items, truncated}`, `--jsonl`
/// is one record per line plus a trailing `{"truncated":true}` line when the list was bounded.
fn print_page<T: Serialize>(
    items: &[T],
    truncated: bool,
    offset: usize,
    json: bool,
    jsonl: bool,
    diagnostics: &[WikiDiagnostic],
) -> CliResult {
    if jsonl {
        print_jsonl(items)?;
        if truncated {
            println!("{}", json!({ "truncated": true }));
        }
        // Diagnostics (a failed refresh) travel as trailing records, and set the exit status.
        let env = envelope_for(&Value::Null, diagnostics);
        for d in &env.diagnostics {
            println!("{}", json!({ "diagnostic": d }));
        }
        let code = env.exit_code();
        if code != exit_code::OK {
            std::process::exit(code);
        }
    } else if json {
        emit(
            &json!({
                "items": items,
                "truncated": truncated,
                "nextOffset": truncated.then_some(offset + items.len()),
            }),
            diagnostics,
        )?;
    }
    Ok(())
}

fn sync_cozo(config: &KnobyteConfig) {
    match crate::cozo::CozoEngine::open_configured(config) {
        Ok(cozo) => {
            if let Ok(conn) = rusqlite::Connection::open(config.wiki_db_path()) {
                if let Err(e) = cozo.sync_from_wiki(&conn) {
                    eprintln!("{} CozoDB sync failed: {}", "[warn]".yellow(), e);
                }
            }
        }
        Err(e) => eprintln!("{} CozoDB not synchronized: {}", "[warn]".yellow(), e),
    }
}

/// Run a wiki subcommand; typed index failures exit with their taxonomy code.
pub fn run_wiki_command(config: &KnobyteConfig, sub: WikiCommands) -> CliResult {
    let json = sub.wants_json();
    match run_wiki_inner(config, sub) {
        Ok(()) => Ok(()),
        Err(e) => {
            let code = e
                .downcast_ref::<rusqlite::Error>()
                .and_then(crate::wiki::index::error_code)
                .map(|c| c.to_string());
            match code {
                Some(c) => {
                    let message = e.to_string();
                    let message = message
                        .strip_prefix(&format!("{}: ", c))
                        .unwrap_or(&message)
                        .to_string();
                    fail(json, &[crate::wiki::diagnostics::diag(&c, message, "")])
                }
                None if json => fail(
                    json,
                    &[crate::wiki::diagnostics::diag("INVALID_REQUEST", e.to_string(), "")],
                ),
                None => Err(e),
            }
        }
    }
}

fn run_wiki_inner(config: &KnobyteConfig, sub: WikiCommands) -> CliResult {
    let scope = WikiScope::load(&config.scaffold_root);
    match sub {
        WikiCommands::List {
            filter,
            json,
            jsonl,
        } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            let page = index.list_filtered(&filter.to_filter())?;
            if json || jsonl {
                print_page(&page.items, page.truncated, filter.offset, json, jsonl, &refresh_diags)?;
            } else {
                for s in &page.items {
                    println!("{}", summary_line(s));
                }
                if page.truncated {
                    println!("{}", "(truncated; raise --limit)".dimmed());
                }
            }
        }
        WikiCommands::Show {
            id,
            no_body,
            limit,
            offset,
            json,
        } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            let opts = crate::wiki::index::DetailOptions {
                include_body: !no_body,
                limit,
                relations_offset: offset,
                backlinks_offset: offset,
                ..Default::default()
            };
            let Some(detail) = index.entity_detail(&id, &opts)? else {
                not_found(json, &id);
            };
            let e = &detail.entity;
            let backlinks: Vec<Value> = detail
                .backlinks
                .items
                .iter()
                .map(|b| json!({ "id": b.id, "title": b.title, "type": b.entity_type }))
                .collect();
            let groundings: Vec<Value> = index
                .groundings_for(&e.id)?
                .into_iter()
                .map(|(r, h, s)| json!({ "ref": r, "health": h, "state": s }))
                .collect();
            if json {
                let mut v = serde_json::to_value(e)?;
                if no_body {
                    if let Some(o) = v.as_object_mut() {
                        o.remove("body");
                    }
                }
                v["backlinks"] = json!(backlinks);
                v["backlinksPage"] = json!({
                    "total": detail.backlinks.total,
                    "truncated": detail.backlinks.truncated,
                    "limit": detail.backlinks.limit,
                    "offset": detail.backlinks.offset,
                    "nextOffset": detail.backlinks.next_offset,
                });
                v["relationsPage"] = json!(detail.relations_page);
                v["groundings"] = json!(groundings);
                emit(&v, &refresh_diags)?;
            } else {
                println!("# {} ({})", e.title.bold(), e.id);
                println!(
                    "Type: {} | Status: {} | Revision: {} | File: {}:{}-{}",
                    e.entity_type, e.status, e.revision, e.file, e.start_line, e.end_line
                );
                if let Some(s) = &e.summary {
                    println!("Summary: {}", s);
                }
                if !e.topics.is_empty() {
                    println!("Topics: {}", e.topics.join(", "));
                }
                for r in &e.relations {
                    println!("  -> {} {}", r.rel_type.cyan(), r.target_id);
                }
                if detail.relations_page.truncated {
                    println!(
                        "{}",
                        format!(
                            "  ({} of {} relations; use --offset {})",
                            e.relations.len(),
                            detail.relations_page.total,
                            detail.relations_page.next_offset.unwrap_or(0)
                        )
                        .dimmed()
                    );
                }
                for b in &backlinks {
                    println!(
                        "  <- {} ({})",
                        b["title"].as_str().unwrap_or(""),
                        b["id"].as_str().unwrap_or("")
                    );
                }
                if detail.backlinks.truncated {
                    println!(
                        "{}",
                        format!(
                            "  ({} of {} backlinks; use --offset {})",
                            backlinks.len(),
                            detail.backlinks.total,
                            detail.backlinks.next_offset.unwrap_or(0)
                        )
                        .dimmed()
                    );
                }
                for g in &groundings {
                    println!(
                        "  grounded: {} [{}]",
                        g["ref"].as_str().unwrap_or(""),
                        g["health"].as_str().unwrap_or("unverified")
                    );
                }
                for s in &e.sources {
                    println!(
                        "  source: {} {}",
                        s.source_type,
                        s.reference.as_deref().or(s.note.as_deref()).unwrap_or("")
                    );
                }
                if !no_body {
                    println!("---\n{}", e.body);
                }
            }
        }
        WikiCommands::Query {
            text,
            filter,
            json,
            jsonl,
        } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            let page = index.search(&text.join(" "), &filter.to_filter())?;
            if json || jsonl {
                print_page(&page.items, page.truncated, filter.offset, json, jsonl, &refresh_diags)?;
            } else {
                for h in &page.items {
                    println!(
                        "{} {}",
                        summary_line(&h.entity),
                        format!("~{}", h.matched).dimmed()
                    );
                }
                if page.truncated {
                    println!("{}", "(truncated; raise --limit)".dimmed());
                }
            }
        }
        WikiCommands::Related {
            id,
            depth,
            max_tokens,
            limit,
            include_archived,
            json,
        } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            match index.neighborhood(&id, depth, max_tokens, limit, include_archived)? {
                None => not_found(json, &id),
                Some(n) if json => emit(&n, &refresh_diags)?,
                Some(n) => {
                    println!("{}", summary_line(&n.origin));
                    for r in &n.relations {
                        println!(
                            "  -> {} {}{}",
                            r.rel_type.cyan(),
                            r.target_id,
                            if r.resolved { "" } else { " (unresolved)" }
                        );
                    }
                    for r in &n.backlinks {
                        println!("  <- {} {}", r.rel_type.cyan(), r.target_id);
                    }
                    for s in &n.reached {
                        println!("  reached: {}", summary_line(s));
                    }
                    if n.truncated {
                        println!("{}", "(truncated by --limit or --max-tokens)".dimmed());
                    }
                }
            }
        }
        WikiCommands::Backlinks {
            id,
            limit,
            offset,
            json,
        } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            let page = index.backlinks_page(&id, limit, offset)?;
            if json {
                emit(&page, &refresh_diags)?;
            } else {
                for e in &page.items {
                    println!("Backlink: {} ({})", e.title.bold(), e.id);
                }
                if page.truncated {
                    println!(
                        "{}",
                        format!(
                            "({} of {}; use --offset {})",
                            page.items.len(),
                            page.total,
                            page.next_offset.unwrap_or(0)
                        )
                        .dimmed()
                    );
                }
            }
        }
        WikiCommands::ForCode {
            node_ids,
            limit,
            include_archived,
            json,
            jsonl,
        } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            let page = index.for_code_filtered(&node_ids, limit, include_archived)?;
            if json || jsonl {
                print_page(&page.items, page.truncated, 0, json, jsonl, &refresh_diags)?;
            } else {
                for h in &page.items {
                    println!(
                        "{} via {} [{}] matched {}",
                        summary_line(&h.entity),
                        h.grounding,
                        h.grounding_health.as_deref().unwrap_or("unverified"),
                        h.matched_nodes.join(", ")
                    );
                }
                if page.truncated {
                    println!("{}", "(truncated; raise --limit)".dimmed());
                }
            }
        }
        WikiCommands::Graph {
            ids,
            depth,
            limit,
            include_archived,
            json,
        } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            let slice = index.graph_slice(&ids, depth, limit, include_archived)?;
            if json {
                emit(&slice, &refresh_diags)?;
            } else {
                for n in &slice.nodes {
                    println!("{}", summary_line(n));
                }
                for e in &slice.edges {
                    println!("  {} -{}-> {}", e.source, e.rel_type.cyan(), e.target);
                }
                if slice.truncated {
                    println!("{}", "(truncated; raise --limit)".dimmed());
                }
            }
        }
        WikiCommands::Trace { id, json } => {
            let (index, refresh_diags) = open_refreshed_index(config)?;
            match crate::wiki::trace::trace(&index, &id, &config.graph_db_path())? {
                None => not_found(json, &id),
                Some(t) if json => emit(&t, &refresh_diags)?,
                Some(t) => {
                    for (ty, nodes) in &t.nodes {
                        for n in nodes {
                            println!(
                                "[{}] {} ({})",
                                ty.cyan(),
                                n.entity.title.bold(),
                                n.entity.id
                            );
                        }
                    }
                    for (e, r, h) in &t.implementations {
                        println!(
                            "  implementation: {} -> {} [{}]",
                            e,
                            r,
                            h.as_deref().unwrap_or("unverified")
                        );
                    }
                    for (r, _, f) in &t.tests {
                        println!("  test: {} called from {}", r, f);
                    }
                    for (e, c, title) in &t.acceptance_criteria {
                        println!("  acceptance: {} verified by {} ({})", e, title, c);
                    }
                    for (e, c, title) in &t.constraints {
                        println!("  constraint: {} constrained by {} ({})", e, title, c);
                    }
                    for g in &t.gaps {
                        println!(
                            "  {} {} {}: {}",
                            "gap".yellow(),
                            g.entity_id,
                            g.hop,
                            g.reason
                        );
                    }
                }
            }
        }
        WikiCommands::Validate {
            json,
            limit,
            strict,
        } => {
            let index = WikiIndex::open(&config.wiki_db_path()).ok();
            let graph_db = config.graph_db_path();
            let report =
                crate::wiki::validate::validate_scaffold(&crate::wiki::validate::ValidateOptions {
                    scope: &scope,
                    project_root: &config.project_root,
                    graph_db: Some(graph_db.as_path()),
                    index: index.as_ref().filter(|i| i.is_built()),
                    limit: Some(limit.unwrap_or(500)),
                });
            if json {
                let mut data = serde_json::to_value(&report)?;
                let status =
                    crate::wiki::maintenance::inspect_index(&config.wiki_db_path(), &scope, true);
                if let Some(o) = data.as_object_mut() {
                    o.remove("diagnostics");
                    o.insert("index".into(), status.state_value());
                }
                return emit(&data, &report.diagnostics);
            } else if report.diagnostics.is_empty() {
                println!(
                    "{} Wiki integrity valid ({} entities in {} files).",
                    "[ok]".green().bold(),
                    report.entities_checked,
                    report.files_scanned
                );
            } else {
                print_diags(&report.diagnostics);
                println!(
                    "{} error(s), {} warning(s), {} info across {} entities{}",
                    report.counts.error,
                    report.counts.warning,
                    report.counts.info,
                    report.entities_checked,
                    if report.truncated {
                        " (truncated; raise --limit)"
                    } else {
                        ""
                    }
                );
            }
            // Error-severity findings always fail the command (exit 1), so CI can gate on it.
            let _ = strict;
            if report.counts.error > 0 {
                let codes: Vec<&str> = report
                    .diagnostics
                    .iter()
                    .filter(|d| d.severity == "error")
                    .map(|d| d.code.as_str())
                    .collect();
                let code = exit_code_for(&codes);
                std::process::exit(if code == exit_code::OK {
                    exit_code::DIAGNOSTICS
                } else {
                    code
                });
            }
        }
        WikiCommands::RebuildIndex { incremental, json } => {
            // The explicit maintenance path: the only one allowed to reset an older index.
            let mut index = WikiIndex::open_for_rebuild(&config.wiki_db_path())?;
            let (count, stats) = if incremental {
                let s = index.refresh(&config.scaffold_root)?;
                (s.entities, Some(s))
            } else {
                (index.rebuild(&config.scaffold_root)?, None)
            };
            drop(index);
            sync_cozo(config);
            if json {
                emit(&json!({ "indexedEntities": count, "refresh": stats }), &[])?;
            } else {
                println!(
                    "{} Rebuilt Wiki search index ({} entities)",
                    "[ok]".green().bold(),
                    count
                );
            }
        }
        WikiCommands::Apply {
            file,
            dry_run,
            json,
        } => {
            let raw = match fs::read_to_string(&file)
                .map_err(|e| format!("Cannot read {}: {}", file, e))
                .and_then(|text| {
                    load_operation_file(&text).map_err(|e| format!("{}: {}", file, e))
                }) {
                Ok(r) => r,
                Err(e) => fail(
                    json,
                    &[crate::wiki::diagnostics::diag("INVALID_OPERATION_ENVELOPE", e, file.clone())],
                ),
            };
            let graph_db = config.graph_db_path();
            let report = apply_operations(
                &raw,
                &ApplyOptions {
                    scope: &scope,
                    graph_db: Some(graph_db.as_path()),
                    dry_run,
                    default_actor: default_actor(config),
                },
            );
            finish_apply(config, &report, json)?;
        }
        WikiCommands::RegenerateViews { dry_run, json } => {
            let report = crate::wiki::views::regenerate_views(&scope, dry_run);
            if !dry_run && report.files.iter().any(|f| f.written) {
                let _ = open_fresh_index(config);
            }
            if json {
                let mut data = serde_json::to_value(&report)?;
                if let Some(o) = data.as_object_mut() {
                    o.remove("diagnostics");
                }
                emit(&data, &report.diagnostics)?;
            } else {
                for f in &report.files {
                    let verb = if f.stale_regions == 0 {
                        "up to date"
                    } else if dry_run {
                        "would be regenerated"
                    } else if f.written {
                        "regenerated"
                    } else {
                        "not written"
                    };
                    println!("{} ({} section(s)): {}", f.file, f.regions, verb);
                }
                if report.files.is_empty() {
                    println!("No generated sections found.");
                }
                print_diags(&report.diagnostics);
            }
        }
        WikiCommands::Synthesis { sub } => run_synthesis(config, &scope, sub)?,
        WikiCommands::Migrate {
            dry_run: _,
            apply,
            json,
        } => run_migrate(config, &scope, apply, json)?,
        WikiCommands::Index { sub } => run_index(config, &scope, sub)?,
    }
    Ok(())
}

fn run_migrate(config: &KnobyteConfig, scope: &WikiScope, apply: bool, json: bool) -> CliResult {
    let graph_db = config.graph_db_path();
    let result = crate::wiki::migrate::migrate(
        &crate::wiki::migrate::MigrationOptions {
            scope,
            graph_db: Some(graph_db.as_path()),
        },
        !apply,
    );
    let mut diags = result.plan.diagnostics.clone();
    if let Some(r) = &result.report {
        diags.extend(r.diagnostics.iter().cloned());
    }
    if result.applied && result.report.as_ref().is_some_and(|r| !r.changed_files.is_empty()) {
        match WikiIndex::open(&config.wiki_db_path()).and_then(|mut i| i.refresh(&config.scaffold_root)) {
            Ok(_) => sync_cozo(config),
            Err(e) => diags.push(crate::wiki::diagnostics::diag(
                "INDEX_REFRESH_REQUIRED",
                format!("The Markdown was migrated but the index did not refresh: {}", e),
                "wiki.db",
            )),
        }
    }
    if apply && result.plan.blocked && !crate::wiki::diagnostics::has_errors(&diags) {
        diags.push(crate::wiki::diagnostics::diag(
            "INVALID_OPERATION_PAYLOAD",
            "The migration plan does not apply cleanly; nothing was written",
            "",
        ));
    }
    if json {
        let mut data = serde_json::to_value(&result)?;
        if let Some(plan) = data.get_mut("plan").and_then(|p| p.as_object_mut()) {
            plan.remove("diagnostics");
        }
        if let Some(report) = data.get_mut("report").and_then(|p| p.as_object_mut()) {
            report.remove("diagnostics");
        }
        return emit(&data, &diags);
    }
    let plan = &result.plan;
    for line in crate::wiki::migrate::render_plan(plan) {
        println!("  {}", line);
    }
    print_diags(
        &diags
            .iter()
            .filter(|d| d.code != "MIGRATION_ABSTAINED")
            .cloned()
            .collect::<Vec<_>>(),
    );
    if plan.items.is_empty() {
        println!(
            "{} The wiki is current: {} file(s), {} entities, nothing to migrate{}.",
            "[ok]".green().bold(),
            plan.files_scanned,
            plan.entities_scanned,
            if plan.abstentions.is_empty() {
                String::new()
            } else {
                format!(" ({} left for review)", plan.abstentions.len())
            }
        );
    } else if plan.blocked {
        println!("{} The migration plan does not apply cleanly; nothing was written.", "[failed]".red().bold());
    } else if !apply {
        println!(
            "{} {} change(s) across {} file(s) planned; nothing written. Run `knobyte wiki migrate --apply`.",
            "[plan]".cyan().bold(),
            plan.items.len(),
            plan.changed_files.len()
        );
    } else if result.applied {
        println!(
            "{} Migrated {} change(s) across {} file(s).",
            "[ok]".green().bold(),
            plan.items.len(),
            plan.changed_files.len()
        );
    } else {
        println!("{} Nothing was written.", "[failed]".red().bold());
    }
    let code = crate::wiki::envelope::envelope_for(&Value::Null, &diags).exit_code();
    if code != exit_code::OK {
        std::process::exit(code);
    }
    Ok(())
}

fn run_index(config: &KnobyteConfig, scope: &WikiScope, sub: IndexCommands) -> CliResult {
    let db = config.wiki_db_path();
    match sub {
        IndexCommands::Status { json } => {
            let status = crate::wiki::maintenance::inspect_index(&db, scope, true);
            if json {
                let mut data = serde_json::to_value(&status)?;
                if let Some(o) = data.as_object_mut() {
                    o.remove("diagnostics");
                    o.insert("index".into(), status.state_value());
                }
                // The state is the answer: only an unreadable index fails the command.
                let diags: Vec<WikiDiagnostic> = status
                    .diagnostics
                    .iter()
                    .cloned()
                    .map(|mut d| {
                        if status.readable() && d.severity == "error" {
                            d.severity = "warning".into();
                        }
                        d
                    })
                    .collect();
                return emit(&data, &diags);
            }
            println!(
                "Wiki index: {}{}",
                status.state.bold(),
                status
                    .indexed_revision
                    .as_deref()
                    .map(|r| format!(" (revision {}, {} entities)", &r[..12.min(r.len())], status.entities))
                    .unwrap_or_default()
            );
            print_diags(&status.diagnostics);
            if !status.readable() {
                std::process::exit(exit_code::INDEX);
            }
        }
        IndexCommands::Dump { out, json } => {
            let index = WikiIndex::open_read_only(&db)?;
            let dump = crate::wiki::maintenance::dump_index(index.connection())?;
            if let Some(path) = &out {
                fs::write(path, &dump)?;
            }
            if json {
                let lines: Vec<&str> = if out.is_some() { Vec::new() } else { dump.lines().collect() };
                return emit(
                    &json!({ "rows": dump.lines().count(), "writtenTo": out, "lines": lines }),
                    &[],
                );
            }
            if out.is_none() {
                print!("{}", dump);
            } else {
                println!("{} Wrote {} row(s) to {}", "[ok]".green().bold(), dump.lines().count(), out.unwrap_or_default());
            }
        }
        IndexCommands::Doctor { json } => {
            let report = crate::wiki::maintenance::doctor(&db, scope);
            let mut diags = report.diagnostics.clone();
            for d in diags.iter_mut() {
                // Doctor reports problems of a readable index as findings; an unreadable one
                // (missing, corrupt, rebuild_required — e.g. a newer schema) fails it, exactly
                // as `index status` does.
                if report.status.readable() && d.severity == "error" && d.code != "WIKI_INDEX_CORRUPT" {
                    d.severity = "warning".into();
                }
            }
            if json {
                let mut data = serde_json::to_value(&report)?;
                if let Some(o) = data.as_object_mut() {
                    o.remove("diagnostics");
                    o.insert("index".into(), report.status.state_value());
                }
                return emit(&data, &diags);
            }
            println!("Wiki index: {}", report.status.state.bold());
            if report.quick_check.is_empty() {
                println!("  integrity: ok");
            } else {
                for q in &report.quick_check {
                    println!("  integrity: {}", q.red());
                }
            }
            match &report.diff {
                Some(d) if d.consistent => println!("  matches a clean rebuild of the Markdown"),
                Some(d) => {
                    println!(
                        "  differs from a clean rebuild: {} row(s) only in the index, {} only in the rebuild",
                        d.only_in_index.len(),
                        d.only_in_rebuild.len()
                    );
                    for l in d.only_in_index.iter().take(10) {
                        println!("    - {}", l);
                    }
                    for l in d.only_in_rebuild.iter().take(10) {
                        println!("    + {}", l);
                    }
                }
                None => println!("  no comparison (index unreadable)"),
            }
            print_diags(&diags);
            if crate::wiki::diagnostics::has_errors(&diags) || !report.status.readable() {
                std::process::exit(exit_code::INDEX);
            }
        }
    }
    Ok(())
}

fn finish_apply(
    config: &KnobyteConfig,
    report: &crate::wiki::ops::ApplyReport,
    json: bool,
) -> CliResult {
    let mut diags = report.diagnostics.clone();
    if report.ok && !report.dry_run && !report.changed_files.is_empty() {
        match WikiIndex::open(&config.wiki_db_path())
            .and_then(|mut i| i.refresh(&config.scaffold_root))
        {
            Ok(_) => sync_cozo(config),
            Err(e) => diags.push(crate::wiki::diagnostics::diag(
                "INDEX_REFRESH_REQUIRED",
                format!(
                    "The Markdown was written but the index did not refresh: {}",
                    e
                ),
                "wiki.db",
            )),
        }
    }
    if json {
        let mut v = serde_json::to_value(report)?;
        if let Some(o) = v.as_object_mut() {
            o.remove("diagnostics");
        }
        if !report.ok && !crate::wiki::diagnostics::has_errors(&diags) {
            diags.push(crate::wiki::diagnostics::diag(
                "INVALID_OPERATION_PAYLOAD",
                "The operations could not be applied; nothing was written",
                "",
            ));
        }
        return emit(&v, &diags);
    } else {
        for op in &report.operations {
            let tag = if op.replayed {
                " (already applied)".dimmed().to_string()
            } else {
                String::new()
            };
            println!(
                "{} {} {}{}",
                op.op_type.cyan(),
                op.op_id,
                op.files.join(", "),
                tag
            );
            for r in &op.revisions {
                println!("  {} revision {} -> {}", r.entity_id, r.before, r.after);
            }
            if report.dry_run {
                for c in &op.changes {
                    println!("  --- {}{}", c.file, if c.created { " (new)" } else { "" });
                    for line in c.diff.lines() {
                        println!("  {}", line);
                    }
                }
            }
        }
        print_diags(&diags);
        if report.ok {
            if report.dry_run {
                println!(
                    "{} Dry run: {} file(s) would change; nothing written.",
                    "[ok]".green().bold(),
                    report.changed_files.len()
                );
            } else {
                println!(
                    "{} Applied; {} file(s) changed.",
                    "[ok]".green().bold(),
                    report.changed_files.len()
                );
            }
        } else {
            println!("{} Nothing was written.", "[failed]".red().bold());
        }
    }
    if !report.ok {
        std::process::exit(apply_exit_code(&diags));
    }
    Ok(())
}

fn apply_exit_code(diags: &[WikiDiagnostic]) -> i32 {
    let codes: Vec<&str> = diags
        .iter()
        .filter(|d| d.severity == "error")
        .map(|d| d.code.as_str())
        .collect();
    match exit_code_for(&codes) {
        exit_code::OK => exit_code::DIAGNOSTICS,
        c => c,
    }
}

fn run_synthesis(config: &KnobyteConfig, scope: &WikiScope, sub: SynthesisCommands) -> CliResult {
    let graph_db = config.graph_db_path();
    let env = SynthesisEnv {
        scope,
        project_root: &config.project_root,
        graph_db: &graph_db,
    };
    match sub {
        SynthesisCommands::Build {
            cluster,
            print,
            json,
        } => {
            let clusters = synthesis::clusters(&env);
            if let Some(c) = &cluster {
                if !clusters.iter().any(|x| &x.name == c) {
                    return Err(format!("No cluster named \"{}\"", c).into());
                }
            }
            let names: Vec<String> = clusters.iter().map(|c| c.name.clone()).collect();
            let playbook = synthesis::render_playbook_with(
                &config.project_root,
                &config.scaffold_root,
                &names,
                cluster.as_deref(),
                &graph_db,
            );
            if json {
                return emit(&json!({ "clusters": clusters, "playbook": playbook }), &[]);
            }
            if print {
                println!("{}", playbook);
                return Ok(());
            }
            let rel = "local/synthesis/playbook.md";
            if let Err(d) = crate::wiki::paths::write_contained(&config.scaffold_root, rel, &playbook) {
                fail(false, std::slice::from_ref(&d));
            }
            let path = config.scaffold_root.join(rel);
            if clusters.is_empty() {
                println!("{}", synthesis::no_clusters_message(&graph_db));
            } else {
                println!("{} cluster(s): {}", clusters.len(), names.join(", "));
            }
            println!(
                "{} Playbook written to {}. Hand it to your coding agent (or use --print).",
                "[ok]".green().bold(),
                path.display()
            );
        }
        SynthesisCommands::Prepare {
            stage,
            cluster,
            json,
        } => {
            let out = match synthesis::prepare(&env, &stage, cluster.as_deref()) {
                Ok(o) => o,
                Err(d) => fail(json, &[d]),
            };
            if json {
                emit(&out, &[])?;
            } else {
                println!(
                    "=== SYSTEM ===\n{}",
                    out["prompt"]["system"].as_str().unwrap_or("")
                );
                println!(
                    "\n=== USER ===\n{}",
                    out["prompt"]["user"].as_str().unwrap_or("")
                );
            }
        }
        SynthesisCommands::Propose {
            file,
            apply,
            stage,
            json,
        } => {
            let text = match fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => fail(
                    json,
                    &[crate::wiki::diagnostics::diag(
                        "INVALID_REQUEST",
                        format!("Cannot read {}: {}", file, e),
                        file.clone(),
                    )],
                ),
            };
            let response: Value = match serde_json::from_str(synthesis::strip_code_fences(&text)) {
                Ok(v) => v,
                Err(e) => fail(
                    json,
                    &[crate::wiki::diagnostics::diag(
                        "INVALID_AGENT_RESPONSE",
                        format!("{} is not JSON: {}", file, e),
                        file.clone(),
                    )],
                ),
            };
            let proposal = match synthesis::propose(&env, &response, stage.as_deref()) {
                Ok(p) => p,
                Err(d) => fail(json, &[d]),
            };
            let report = if proposal.operations.is_empty() {
                None
            } else {
                Some(apply_operations(
                    &proposal.operations,
                    &ApplyOptions {
                        scope,
                        graph_db: Some(graph_db.as_path()),
                        dry_run: !apply,
                        default_actor: OpActor {
                            kind: "agent".into(),
                            id: "synthesis".into(),
                            session_id: None,
                        },
                    },
                ))
            };
            if json {
                if let Some(r) = &report {
                    if r.ok && apply && !r.changed_files.is_empty() {
                        let _ = open_fresh_index(config);
                    }
                }
                let diags = report.as_ref().map(|r| r.diagnostics.clone()).unwrap_or_default();
                let mut plan = serde_json::to_value(&report)?;
                if let Some(o) = plan.as_object_mut() {
                    o.remove("diagnostics");
                }
                return emit(&json!({ "proposal": proposal, "plan": plan }), &diags);
            }
            println!(
                "Stage {}: {} accepted, {} rejected, {} skipped",
                proposal.stage,
                proposal.accepted,
                proposal.rejected.len(),
                proposal.skipped
            );
            for r in &proposal.rejected {
                let title = r.item.get("title").and_then(|v| v.as_str()).unwrap_or("");
                println!(
                    "  {} {} {}",
                    "rejected".yellow(),
                    title,
                    r.reasons.join("; ")
                );
            }
            match report {
                None => println!("Nothing to propose."),
                Some(r) => finish_apply(config, &r, false)?,
            }
        }
    }
    Ok(())
}

/// `knobyte export [--out PATH]`.
pub fn run_export(config: &KnobyteConfig, out: Option<String>) -> CliResult {
    let result = crate::wiki::export::export_scaffold(
        &config.project_root,
        &config.scaffold_root,
        out.as_deref(),
    )?;
    match &result.written_to {
        Some(_) => println!(
            "Wrote {} scaffold file(s) to {}",
            result.files.len(),
            out.unwrap_or_default()
        ),
        None => print!("{}", result.document),
    }
    Ok(())
}
