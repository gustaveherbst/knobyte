use clap::{Parser, Subcommand};
use colored::Colorize;
use std::fs;

use knobyte::capabilities::get_capabilities;
use knobyte::config::find_config;
use knobyte::cozo::CozoEngine;
use knobyte::doctor::run_doctor;
use knobyte::drift::{plan_sync, run_drift_check};
use knobyte::events::{append_event, query_timeline, TimelineFilter};
use knobyte::graph::{scan_indexable_files, GraphEngine};
use knobyte::heartbeat::check_heartbeat;
use knobyte::hub::start_hub_server;
use knobyte::mcp::{start_sse_server, start_stdio_server};
use knobyte::progress::{format_bytes, IndexProgressBar};
use knobyte::setup::run_setup;
use knobyte::skills::sync_skills;
use knobyte::team::activity::{list_activity, record_activity};
use knobyte::team::inbox::{
    delete_inbox_draft, list_inbox_drafts, list_inbox_proposals, publish_inbox_draft,
    save_inbox_draft, InboxDraft,
};
use knobyte::team::members::{
    clear_current_member, get_current_member, list_members, select_current_member,
};
use knobyte::team::relay::{
    acknowledge_relay, close_relay, delete_relay_draft, list_relay_drafts, list_relays,
    publish_relay_draft, save_relay_draft, RelayDraft,
};
use knobyte::team::specs::list_specs;
use knobyte::team::workstreams::{get_workstream, list_workstreams, save_workstream, Workstream};
use knobyte::wiki::WikiIndex;

#[derive(Parser)]
#[command(name = "knobyte", version = knobyte::VERSION, about = "Persistent project memory and deterministic code graphs for AI coding agents - 100% Rust")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the local Project Hub web interface (or setup if unconfigured)
    Hub {
        #[arg(long, default_value_t = 3000)]
        port: u16,
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        #[arg(long = "no-open")]
        no_open: bool,
    },
    /// CozoDB neuro-symbolic Datalog graph and HNSW vector engine
    Cozo {
        #[command(subcommand)]
        sub: CozoCommands,
    },
    /// Set up Knobyte project memory in the repository
    Setup {
        #[arg(long)]
        cli: bool,
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long, default_value = "code-repo")]
        mode: String,
        #[arg(long, default_value_t = 3000)]
        port: u16,
        #[arg(long = "no-open")]
        no_open: bool,
    },
    /// Check project memory drift against current codebase
    Check {
        #[arg(long)]
        quiet: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        fix: bool,
    },
    /// Plan or apply AI drift synchronization
    Sync {
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        warnings: bool,
    },
    /// Build or query the deterministic code graph
    Graph {
        #[command(subcommand)]
        sub: GraphCommands,
    },
    /// Find blast radius / impact of a symbol or file
    Impact {
        target: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jsonl: bool,
    },
    /// Build, query, and validate the project Wiki
    Wiki {
        #[command(subcommand)]
        sub: WikiCommands,
    },
    /// Manage team member attribution and identity
    Member {
        #[command(subcommand)]
        sub: MemberCommands,
    },
    /// View canonical Activity history
    Activity {
        #[command(subcommand)]
        sub: ActivityCommands,
    },
    /// Manage team workstreams
    Workstream {
        #[command(subcommand)]
        sub: WorkstreamCommands,
    },
    /// List or view requirements specs
    Spec {
        #[command(subcommand)]
        sub: SpecCommands,
    },
    /// Propose additions or corrections to project memory
    Inbox {
        #[command(subcommand)]
        sub: InboxCommands,
    },
    /// Prepare and exchange context handoffs (relays)
    Relay {
        #[command(subcommand)]
        sub: RelayCommands,
    },
    /// Append a note, decision, discovery, risk, or todo to the event log
    Log {
        message: String,
        #[arg(long, default_value = "note")]
        kind: String,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "file")]
        files: Vec<String>,
    },
    /// Search recent event log entries and project notes
    Timeline {
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Run lightweight agent-memory health checks
    Heartbeat {
        #[arg(long)]
        json: bool,
    },
    /// Comprehensive health diagnostic summary
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Sync official agent skills for Claude Code and Codex
    Skills {
        #[command(subcommand)]
        sub: SkillCommands,
    },
    /// Structured capability discovery for AI agents
    Capabilities {
        #[arg(long)]
        json: bool,
    },
    /// Start the Model Context Protocol (MCP) server
    Mcp {
        #[arg(long, default_value_t = 3001)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long)]
        sse: bool,
        #[arg(long)]
        stdio: bool,
    },
    /// Create a new pattern template
    Pattern {
        #[command(subcommand)]
        sub: PatternCommands,
    },
    /// Print list of all available commands
    Commands,
}

#[derive(Subcommand)]
enum CozoCommands {
    /// Execute an arbitrary CozoScript Datalog query
    Query {
        script: String,
        #[arg(long)]
        params: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// HNSW vector similarity search on code nodes or wiki entities
    Search {
        query: String,
        #[arg(long, default_value = "code")]
        target: String,
        #[arg(long, default_value_t = 10)]
        k: usize,
        #[arg(long)]
        json: bool,
    },
    /// Compute PageRank centrality scores on code dependency graph
    Pagerank {
        #[arg(long, default_value_t = 0.85)]
        theta: f64,
        #[arg(long, default_value_t = 20)]
        iterations: usize,
        #[arg(long)]
        json: bool,
    },
    /// Find shortest path between two code nodes
    ShortestPath {
        start: String,
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Synchronize SQLite graph.db and wiki.db into CozoDB
    Sync {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum GraphCommands {
    Status {
        #[arg(long)]
        json: bool,
    },
    Refresh {
        #[arg(long)]
        json: bool,
    },
    Rebuild {
        #[arg(long)]
        json: bool,
    },
    Query {
        relation: String,
        target: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jsonl: bool,
    },
    Scope {
        tasks: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jsonl: bool,
    },
    Get {
        ids: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jsonl: bool,
    },
    Ground,
    Repair,
}

#[derive(Subcommand)]
enum WikiCommands {
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Query {
        text: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Related {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Backlinks {
        id: String,
        #[arg(long)]
        json: bool,
    },
    ForCode {
        node_id: String,
        #[arg(long)]
        json: bool,
    },
    Validate {
        #[arg(long)]
        json: bool,
    },
    RebuildIndex {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum MemberCommands {
    List {
        #[arg(long)]
        json: bool,
    },
    Current {
        #[arg(long)]
        json: bool,
    },
    Select {
        id: String,
    },
    Clear,
}

#[derive(Subcommand)]
enum ActivityCommands {
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Record {
        action: String,
        summary: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkstreamCommands {
    List {
        #[arg(long)]
        json: bool,
    },
    Create {
        id: String,
        title: String,
        #[arg(long)]
        description: Option<String>,
    },
    Archive {
        id: String,
    },
}

#[derive(Subcommand)]
enum SpecCommands {
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum InboxCommands {
    Draft {
        #[command(subcommand)]
        sub: InboxDraftCommands,
    },
    Publish {
        draft_id: String,
    },
    Proposal {
        #[command(subcommand)]
        sub: InboxProposalCommands,
    },
}

#[derive(Subcommand)]
enum InboxDraftCommands {
    List {
        #[arg(long)]
        json: bool,
    },
    Save {
        #[arg(long)]
        title: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        content: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "engineer")]
        author: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand)]
enum InboxProposalCommands {
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum RelayCommands {
    Draft {
        #[command(subcommand)]
        sub: RelayDraftCommands,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Publish {
        draft_id: String,
    },
    Acknowledge {
        relay_id: String,
        #[arg(long)]
        member: Option<String>,
    },
    Close {
        relay_id: String,
        #[arg(long)]
        member: Option<String>,
    },
}

#[derive(Subcommand)]
enum RelayDraftCommands {
    List {
        #[arg(long)]
        json: bool,
    },
    Save {
        #[arg(long)]
        title: String,
        #[arg(long)]
        summary: String,
        #[arg(long, default_value = "engineer")]
        sender: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    Sync {
        #[arg(long)]
        tool: Option<String>,
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PatternCommands {
    Add { name: String },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = find_config(None).unwrap_or_else(|_| {
        let cwd = std::env::current_dir().unwrap();
        knobyte::config::KnobyteConfig::new(cwd.clone(), cwd.join(".knobyte"))
    });

    match cli.command {
        None => {
            if !config.scaffold_root.exists() {
                println!(
                    "{}",
                    "No Knobyte scaffold found. Running setup first...".yellow()
                );
                run_setup(&config, "code-repo", false)?;
            }
            start_hub_server(config, "0.0.0.0", 3000, true).await?;
        }
        Some(Commands::Hub {
            port,
            host,
            no_open,
        }) => {
            if !config.scaffold_root.exists() {
                println!(
                    "{}",
                    "No Knobyte scaffold found. Running setup first...".yellow()
                );
                run_setup(&config, "code-repo", false)?;
            }
            start_hub_server(config, &host, port, !no_open).await?;
        }
        Some(Commands::Setup { dry_run, mode, .. }) => {
            run_setup(&config, &mode, dry_run)?;
            println!(
                "{} Initialized Knobyte scaffold at {}",
                "[ok]".green().bold(),
                config.scaffold_root.display()
            );
        }
        Some(Commands::Check { quiet, json, fix }) => {
            if fix {
                let sync_res = knobyte::drift::sync_groundings(&config, false)?;
                if sync_res.relocated_count > 0 && !quiet && !json {
                    println!(
                        "{} Automatically relocated {} moved grounding anchor(s).",
                        "[ok]".green().bold(),
                        sync_res.relocated_count
                    );
                }
            }
            let report = run_drift_check(&config);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if quiet {
                println!(
                    "Drift Score: {:.1}% ({} files, {} issues)",
                    report.score, report.file_count, report.issue_count
                );
            } else {
                println!("{}", "=== Knobyte Drift Report ===".bold());
                println!("Score: {:.1}%", report.score);
                println!("Scaffold Files: {}", report.file_count);
                println!(
                    "Groundings: {} intact, {} changed, {} missing",
                    report.grounding.intact, report.grounding.changed, report.grounding.missing
                );
                if !report.issues.is_empty() {
                    println!("\n{}", "Issues:".yellow());
                    for issue in &report.issues {
                        println!("  - [{}] {}: {}", issue.severity, issue.file, issue.message);
                    }
                } else {
                    println!(
                        "{} All scaffold files pristine and in sync with codebase.",
                        "[ok]".green().bold()
                    );
                }
            }
        }
        Some(Commands::Sync { dry_run, warnings }) => {
            let sync_res = knobyte::drift::sync_groundings(&config, dry_run)?;
            if !sync_res.proposals.is_empty() {
                println!("{}", "=== Grounding Anchor Relocations ===".bold());
                for prop in &sync_res.proposals {
                    println!(
                        "  - [{}] {}: {} -> {} (confidence: {:.0}%)",
                        prop.scaffold_file.cyan(),
                        prop.symbol_name.bold(),
                        prop.old_file
                            .as_deref()
                            .unwrap_or(&prop.old_node_id)
                            .dimmed(),
                        prop.new_file.green(),
                        prop.confidence * 100.0
                    );
                    println!("    Reason: {}", prop.reason.dimmed());
                }
                println!();
            }

            if !dry_run && sync_res.relocated_count > 0 {
                println!(
                    "{} Relocated and healed {} grounding anchor(s) in scaffold files.",
                    "[ok]".green().bold(),
                    sync_res.relocated_count
                );
            } else if dry_run && !sync_res.proposals.is_empty() {
                println!(
                    "{} Dry-run: {} anchor(s) eligible for automatic relocation. Run without --dry-run to apply.",
                    "[info]".cyan().bold(),
                    sync_res.proposals.len()
                );
            }

            let plan = plan_sync(&config, warnings);
            if plan.clean {
                if sync_res.proposals.is_empty() {
                    println!(
                        "{} No drift detected. Project memory is up to date.",
                        "[ok]".green().bold()
                    );
                }
            } else {
                println!(
                    "Additional Drift Sync Actions ({} actions):",
                    plan.actions.len()
                );
                for act in plan.actions {
                    println!("\nFile: {}", act.file.bold());
                    println!("Recommendation: {}", act.recommendation);
                    if dry_run {
                        println!("AI Prompt:\n{}", act.prompt.dimmed());
                    }
                }
            }
        }
        Some(Commands::Graph { sub }) => {
            let db_path = config.graph_db_path();
            let mut engine = GraphEngine::open(&db_path)?;

            match sub {
                GraphCommands::Status { json } => {
                    let st = engine.status()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&st)?);
                    } else {
                        println!("{}", "=== Code Graph Status ===".bold());
                        println!("Nodes: {}", st.node_count);
                        println!("Edges: {}", st.edge_count);
                        println!("Files: {}", st.file_count);
                        println!("Unresolved Refs: {}", st.unresolved_count);
                        println!("Schema Version: {}", st.schema_version);
                        if let Some(t) = st.last_indexed {
                            println!("Last Built: {}", t);
                        }
                    }
                }
                GraphCommands::Refresh { json } | GraphCommands::Rebuild { json } => {
                    let (files, total_bytes) = scan_indexable_files(&config.project_root);
                    if !json {
                        println!(
                            "Indexing code repository at {} ({} files, {})...",
                            config.project_root.display(),
                            files.len(),
                            format_bytes(total_bytes)
                        );
                    }
                    let progress = IndexProgressBar::new(total_bytes, files.len(), !json);
                    let summary =
                        engine.rebuild_files(&config.project_root, &files, Some(&progress))?;
                    if let Ok(cozo) = CozoEngine::open(config.cozo_db_path()) {
                        if let Ok(conn) = rusqlite::Connection::open(config.graph_db_path()) {
                            progress.set_phase("Synchronizing graph to CozoDB...");
                            let _ = cozo.sync_from_graph(&conn);
                        }
                    }
                    progress.finish_and_clear();
                    if json {
                        println!("{}", serde_json::to_string_pretty(&summary)?);
                    } else {
                        println!(
                            "{} Built code graph in {}ms ({} files, {} symbols, {} relationships)",
                            "[ok]".green().bold(),
                            summary.duration_ms,
                            summary.files_indexed,
                            summary.nodes_indexed,
                            summary.edges_indexed
                        );
                    }
                }
                GraphCommands::Query {
                    relation,
                    target,
                    json,
                    jsonl,
                } => {
                    let nodes = match relation.as_str() {
                        "where-defined" => engine.query_where_defined(&target)?,
                        "who-calls" => engine.query_who_calls(&target)?,
                        "who-imports" => engine.query_who_imports(&target)?,
                        _ => return Err(format!(
                            "Unknown relation: {}. Use where-defined, who-calls, or who-imports",
                            relation
                        )
                        .into()),
                    };
                    if jsonl {
                        for n in &nodes {
                            println!("{}", serde_json::to_string(n)?);
                        }
                    } else if json {
                        println!("{}", serde_json::to_string_pretty(&nodes)?);
                    } else {
                        for n in nodes {
                            println!(
                                "{} {} ({}:{}:{})",
                                n.kind.cyan(),
                                n.qualified_name.bold(),
                                n.file_path,
                                n.start_line,
                                n.start_column
                            );
                        }
                    }
                }
                GraphCommands::Scope { tasks, json, jsonl } => {
                    let task_str = tasks.join(" ");
                    let nodes = engine.query_scope(&task_str)?;
                    if jsonl {
                        for n in &nodes {
                            println!("{}", serde_json::to_string(n)?);
                        }
                    } else if json {
                        println!("{}", serde_json::to_string_pretty(&nodes)?);
                    } else {
                        for n in nodes {
                            println!(
                                "{} {} ({}:{})",
                                n.kind.cyan(),
                                n.qualified_name.bold(),
                                n.file_path,
                                n.start_line
                            );
                        }
                    }
                }
                GraphCommands::Get { ids, json, jsonl } => {
                    let nodes = engine.get_nodes(&ids)?;
                    if jsonl {
                        for n in &nodes {
                            println!("{}", serde_json::to_string(n)?);
                        }
                    } else if json {
                        println!("{}", serde_json::to_string_pretty(&nodes)?);
                    } else {
                        for n in nodes {
                            println!("{} {} ({})", n.kind.cyan(), n.name.bold(), n.id);
                            if let Some(sig) = n.signature {
                                println!("  {}", sig.dimmed());
                            }
                        }
                    }
                }
                GraphCommands::Ground => {
                    let count = engine.ground_all(&config.project_root)?;
                    println!(
                        "{} Grounded {} symbols to baseline.",
                        "[ok]".green().bold(),
                        count
                    );
                }
                GraphCommands::Repair => {
                    engine.repair()?;
                    println!(
                        "{} Graph database checkpointed and verified.",
                        "[ok]".green().bold()
                    );
                }
            }
        }
        Some(Commands::Impact {
            target,
            json,
            jsonl,
        }) => {
            let engine = GraphEngine::open(&config.graph_db_path())?;
            let nodes = engine.query_impact(&target)?;
            if jsonl {
                for n in &nodes {
                    println!("{}", serde_json::to_string(n)?);
                }
            } else if json {
                println!("{}", serde_json::to_string_pretty(&nodes)?);
            } else {
                println!(
                    "Impact radius for '{}': {} affected nodes",
                    target,
                    nodes.len()
                );
                for n in nodes {
                    println!(
                        "  - {} {} ({})",
                        n.kind.cyan(),
                        n.qualified_name,
                        n.file_path
                    );
                }
            }
        }
        Some(Commands::Wiki { sub }) => {
            let db_path = config.wiki_db_path();
            let mut index = WikiIndex::open(&db_path)?;

            match sub {
                WikiCommands::List { json } => {
                    let entities = index.list()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&entities)?);
                    } else {
                        for e in entities {
                            println!("[{}] {} ({})", e.entity_type.cyan(), e.title.bold(), e.id);
                        }
                    }
                }
                WikiCommands::Show { id, json } => {
                    if let Some(e) = index.show(&id)? {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&e)?);
                        } else {
                            println!("# {} ({})", e.title.bold(), e.id);
                            println!(
                                "Type: {} | Status: {} | File: {}",
                                e.entity_type, e.status, e.file
                            );
                            if let Some(s) = e.summary {
                                println!("Summary: {}\n", s);
                            }
                            println!("---\n{}", e.body);
                        }
                    } else {
                        println!("Entity '{}' not found", id);
                    }
                }
                WikiCommands::Query { text, json } => {
                    let query_str = text.join(" ");
                    let entities = index.query(&query_str)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&entities)?);
                    } else {
                        for e in entities {
                            println!("[{}] {} ({})", e.entity_type.cyan(), e.title.bold(), e.file);
                        }
                    }
                }
                WikiCommands::Related { id, json } => {
                    let entities = index.related(&id)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&entities)?);
                    } else {
                        for e in entities {
                            println!("Related: {} ({})", e.title.bold(), e.id);
                        }
                    }
                }
                WikiCommands::Backlinks { id, json } => {
                    let entities = index.backlinks(&id)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&entities)?);
                    } else {
                        for e in entities {
                            println!("Backlink: {} ({})", e.title.bold(), e.id);
                        }
                    }
                }
                WikiCommands::ForCode { node_id, json } => {
                    let entities = index.for_code(&node_id)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&entities)?);
                    } else {
                        for e in entities {
                            println!("Grounded: {} ({})", e.title.bold(), e.id);
                        }
                    }
                }
                WikiCommands::Validate { json } => {
                    let diags = index.validate()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&diags)?);
                    } else if diags.is_empty() {
                        println!(
                            "{} Wiki integrity valid. No broken references.",
                            "[ok]".green().bold()
                        );
                    } else {
                        for d in diags {
                            println!("! [{}]: {} ({})", d.code.yellow(), d.message, d.file);
                        }
                    }
                }
                WikiCommands::RebuildIndex { json } => {
                    let count = index.rebuild(&config.scaffold_root)?;
                    if let Ok(cozo) = CozoEngine::open(config.cozo_db_path()) {
                        if let Ok(conn) = rusqlite::Connection::open(config.wiki_db_path()) {
                            let _ = cozo.sync_from_wiki(&conn);
                        }
                    }
                    if json {
                        println!("{}", serde_json::json!({ "indexedEntities": count }));
                    } else {
                        println!(
                            "{} Rebuilt Wiki search index ({} entities)",
                            "[ok]".green().bold(),
                            count
                        );
                    }
                }
            }
        }
        Some(Commands::Member { sub }) => match sub {
            MemberCommands::List { json } => {
                let members = list_members(&config);
                if json {
                    println!("{}", serde_json::to_string_pretty(&members)?);
                } else {
                    for m in members {
                        println!("- {} ({}) [{}]", m.display_name.bold(), m.id, m.status);
                    }
                }
            }
            MemberCommands::Current { json } => {
                let cur = get_current_member(&config);
                if json {
                    println!("{}", serde_json::to_string_pretty(&cur)?);
                } else if let Some(m) = cur {
                    println!("Current member: {} ({})", m.display_name.bold(), m.id);
                } else {
                    println!("No current member selected.");
                }
            }
            MemberCommands::Select { id } => {
                select_current_member(&config, &id)?;
                println!("{} Selected member '{}'", "[ok]".green().bold(), id);
            }
            MemberCommands::Clear => {
                clear_current_member(&config)?;
                println!("{} Cleared current member selection", "[ok]".green().bold());
            }
        },
        Some(Commands::Activity { sub }) => match sub {
            ActivityCommands::List { limit, json } => {
                let acts = list_activity(&config, limit);
                if json {
                    println!("{}", serde_json::to_string_pretty(&acts)?);
                } else {
                    for a in acts {
                        println!(
                            "[{}] {}: {} ({})",
                            a.timestamp.dimmed(),
                            a.actor.bold(),
                            a.summary,
                            a.action
                        );
                    }
                }
            }
            ActivityCommands::Record {
                action,
                summary,
                kind,
                target,
            } => {
                let actor = get_current_member(&config)
                    .map(|m| m.display_name)
                    .unwrap_or_else(|| "engineer".to_string());
                let entity_kind = kind.unwrap_or_else(|| "general".to_string());
                let entity_id = target.clone().unwrap_or_default();
                let entity_title = target.unwrap_or_else(|| "Repository".to_string());
                let rec = record_activity(
                    &config,
                    &actor,
                    &action,
                    &entity_kind,
                    &entity_id,
                    &entity_title,
                    &summary,
                    None,
                )?;
                println!(
                    "{} Recorded activity: {}",
                    "[ok]".green().bold(),
                    rec.summary
                );
            }
        },
        Some(Commands::Workstream { sub }) => match sub {
            WorkstreamCommands::List { json } => {
                let ws = list_workstreams(&config);
                if json {
                    println!("{}", serde_json::to_string_pretty(&ws)?);
                } else {
                    for w in ws {
                        println!("- {} ({}) [{}]", w.title.bold(), w.id, w.status);
                    }
                }
            }
            WorkstreamCommands::Create {
                id,
                title,
                description,
            } => {
                let now = chrono::Utc::now().to_rfc3339();
                let ws = Workstream {
                    id: id.clone(),
                    title,
                    description,
                    status: "active".to_string(),
                    owner: get_current_member(&config).map(|m| m.id),
                    steps: Vec::new(),
                    checkpoints: Vec::new(),
                    created_at: now.clone(),
                    updated_at: now,
                };
                save_workstream(&config, &ws)?;
                println!("{} Created workstream '{}'", "[ok]".green().bold(), id);
            }
            WorkstreamCommands::Archive { id } => {
                if let Some(mut ws) = get_workstream(&config, &id) {
                    ws.status = "archived".to_string();
                    ws.updated_at = chrono::Utc::now().to_rfc3339();
                    save_workstream(&config, &ws)?;
                    println!("{} Archived workstream '{}'", "[ok]".green().bold(), id);
                } else {
                    println!("Workstream '{}' not found", id);
                }
            }
        },
        Some(Commands::Spec { sub }) => match sub {
            SpecCommands::List { json } => {
                let specs = list_specs(&config);
                if json {
                    println!("{}", serde_json::to_string_pretty(&specs)?);
                } else {
                    for s in specs {
                        println!("- {} ({}) [{}]", s.title.bold(), s.id, s.status);
                    }
                }
            }
        },
        Some(Commands::Inbox { sub }) => match sub {
            InboxCommands::Draft { sub: draft_cmd } => match draft_cmd {
                InboxDraftCommands::List { json } => {
                    let drafts = list_inbox_drafts(&config);
                    if json {
                        println!("{}", serde_json::to_string_pretty(&drafts)?);
                    } else {
                        for d in drafts {
                            println!("- [{}] {} ({})", d.target.cyan(), d.title.bold(), d.id);
                        }
                    }
                }
                InboxDraftCommands::Save {
                    title,
                    target,
                    content,
                    reason,
                    author,
                } => {
                    let draft = InboxDraft {
                        id: format!("draft_{}", uuid::Uuid::new_v4()),
                        target,
                        title,
                        proposed_content: content,
                        reason,
                        author,
                        created_at: chrono::Utc::now().to_rfc3339(),
                    };
                    save_inbox_draft(&config, &draft)?;
                    println!("{} Saved inbox draft: {}", "[ok]".green().bold(), draft.id);
                }
                InboxDraftCommands::Delete { id } => {
                    delete_inbox_draft(&config, &id)?;
                    println!("{} Deleted inbox draft '{}'", "[ok]".green().bold(), id);
                }
            },
            InboxCommands::Publish { draft_id } => {
                let prop = publish_inbox_draft(&config, &draft_id)?;
                println!(
                    "{} Published inbox proposal '{}': {}",
                    "[ok]".green().bold(),
                    prop.id,
                    prop.title
                );
            }
            InboxCommands::Proposal { sub: prop_cmd } => match prop_cmd {
                InboxProposalCommands::List { json } => {
                    let props = list_inbox_proposals(&config);
                    if json {
                        println!("{}", serde_json::to_string_pretty(&props)?);
                    } else {
                        for p in props {
                            println!(
                                "- [{}] {} ({}) - {}",
                                p.status.cyan(),
                                p.title.bold(),
                                p.id,
                                p.target
                            );
                        }
                    }
                }
            },
        },
        Some(Commands::Relay { sub }) => match sub {
            RelayCommands::Draft { sub: draft_cmd } => match draft_cmd {
                RelayDraftCommands::List { json } => {
                    let drafts = list_relay_drafts(&config);
                    if json {
                        println!("{}", serde_json::to_string_pretty(&drafts)?);
                    } else {
                        for d in drafts {
                            println!("- {} ({}) - {}", d.title.bold(), d.id, d.summary);
                        }
                    }
                }
                RelayDraftCommands::Save {
                    title,
                    summary,
                    sender,
                } => {
                    let draft = RelayDraft {
                        id: format!("draft_{}", uuid::Uuid::new_v4()),
                        title,
                        summary,
                        sender,
                        open_to_team: true,
                        named_recipients: Vec::new(),
                        progress: Vec::new(),
                        blockers: Vec::new(),
                        next_actions: Vec::new(),
                        evidence: Vec::new(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                    };
                    save_relay_draft(&config, &draft)?;
                    println!("{} Saved relay draft: {}", "[ok]".green().bold(), draft.id);
                }
                RelayDraftCommands::Delete { id } => {
                    delete_relay_draft(&config, &id)?;
                    println!("{} Deleted relay draft '{}'", "[ok]".green().bold(), id);
                }
            },
            RelayCommands::List { json } => {
                let relays = list_relays(&config);
                if json {
                    println!("{}", serde_json::to_string_pretty(&relays)?);
                } else {
                    for r in relays {
                        println!(
                            "- [{}] {} ({}) by {}",
                            r.status.cyan(),
                            r.title.bold(),
                            r.id,
                            r.sender
                        );
                    }
                }
            }
            RelayCommands::Publish { draft_id } => {
                let relay = publish_relay_draft(&config, &draft_id)?;
                println!(
                    "{} Published relay '{}': {}",
                    "[ok]".green().bold(),
                    relay.id,
                    relay.title
                );
            }
            RelayCommands::Acknowledge { relay_id, member } => {
                let member_id = member
                    .or_else(|| get_current_member(&config).map(|m| m.id))
                    .ok_or_else(|| "No member specified or selected".to_string())?;
                let r = acknowledge_relay(&config, &relay_id, &member_id)?;
                println!(
                    "{} Claimed relay '{}' by {}",
                    "[ok]".green().bold(),
                    r.id,
                    member_id
                );
            }
            RelayCommands::Close { relay_id, member } => {
                let member_id = member
                    .or_else(|| get_current_member(&config).map(|m| m.id))
                    .ok_or_else(|| "No member specified or selected".to_string())?;
                let r = close_relay(&config, &relay_id, &member_id)?;
                println!("{} Closed relay '{}'", "[ok]".green().bold(), r.id);
            }
        },
        Some(Commands::Log {
            message,
            kind,
            tags,
            files,
        }) => {
            let actor = get_current_member(&config).map(|m| m.display_name);
            let entry = append_event(&config, &message, &kind, &tags, &files, actor.as_deref())?;
            println!(
                "{} Logged {}: {}",
                "[ok]".green().bold(),
                entry.kind.cyan(),
                entry.summary
            );
        }
        Some(Commands::Timeline {
            query,
            kind,
            file,
            limit,
            json,
        }) => {
            let filter = TimelineFilter {
                query,
                kind,
                file,
                since: None,
                include_superseded: false,
                limit,
            };
            let resp = query_timeline(&config, filter);
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                for e in resp.entries {
                    println!(
                        "[{}] {} - {}",
                        e.timestamp.dimmed(),
                        e.kind.cyan(),
                        e.summary.bold()
                    );
                }
            }
        }
        Some(Commands::Heartbeat { json }) => {
            let report = check_heartbeat(&config, 14);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.ok {
                println!(
                    "{} Scaffold heartbeat healthy. No stale files detected.",
                    "[ok]".green().bold()
                );
            } else {
                println!("{}", "Heartbeat report:".yellow());
                for sf in report.stale_files {
                    println!("  - Stale file: {} ({} days)", sf.path, sf.age_days);
                }
            }
        }
        Some(Commands::Doctor { json }) => {
            let doc = run_doctor(&config);
            if json {
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                println!("{}", "=== Knobyte Health Diagnostic ===".bold());
                println!("Overall Health: {}", doc.overall_health.bold());
                println!(
                    "Git Repository: {}",
                    if doc.git_repository {
                        "yes".green()
                    } else {
                        "no".red()
                    }
                );
                println!("Scaffold Root:  {}", config.scaffold_root.display());
                println!(
                    "Code Graph:     {} ({} nodes, {} edges)",
                    if doc.graph_db_ready {
                        "ready".green()
                    } else {
                        "not built".yellow()
                    },
                    doc.graph_nodes,
                    doc.graph_edges
                );
                println!(
                    "Wiki Index:     {} ({} entities)",
                    if doc.wiki_db_ready {
                        "ready".green()
                    } else {
                        "not built".yellow()
                    },
                    doc.wiki_entities
                );
                println!(
                    "CozoDB Engine:  {}",
                    if doc.cozodb_ready {
                        "ready (Datalog + HNSW Vector)".green()
                    } else {
                        "not initialized".yellow()
                    }
                );
                if !doc.diagnostics.is_empty() {
                    println!("\nDiagnostics:");
                    for d in doc.diagnostics {
                        println!("  ! {}", d.yellow());
                    }
                }
            }
        }
        Some(Commands::Skills { sub }) => match sub {
            SkillCommands::Sync {
                tool,
                dry_run,
                json,
            } => {
                let rep = sync_skills(&config, tool.as_deref(), dry_run)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rep)?);
                } else {
                    for act in rep.actions {
                        println!(
                            "{} {} [{}] -> {}",
                            "[ok]".green().bold(),
                            act.client,
                            act.skill_name,
                            act.path
                        );
                    }
                }
            }
        },
        Some(Commands::Capabilities { json }) => {
            let caps = get_capabilities(&config);
            if json {
                println!("{}", serde_json::to_string_pretty(&caps)?);
            } else {
                println!("Capabilities schema v{}", caps.schema_version);
                for c in caps.capabilities {
                    println!("  - {}: {}", c.id, c.availability);
                }
            }
        }
        Some(Commands::Mcp {
            port, host, stdio, ..
        }) => {
            if stdio {
                start_stdio_server()?;
            } else {
                start_sse_server(&host, port).await?;
            }
        }
        Some(Commands::Pattern { sub }) => match sub {
            PatternCommands::Add { name } => {
                let dir = config.patterns_dir();
                fs::create_dir_all(&dir)?;
                let file_name = format!("{}.md", name.to_lowercase().replace(' ', "-"));
                let path = dir.join(file_name);
                let content = format!(
                    r#"---
id: kb_pattern_{name_clean}
title: Pattern: {name}
type: pattern
status: draft
revision: 1
---

# Pattern: {name}

## Intent
Describe what problem this pattern solves.

## Context & Constraints
Describe when to apply this pattern.

## Code Grounding
Link to reference implementations.
"#,
                    name_clean = name.to_lowercase().replace(' ', "_"),
                    name = name
                );
                fs::write(&path, content)?;
                println!(
                    "{} Created pattern file: {}",
                    "[ok]".green().bold(),
                    path.display()
                );
            }
        },
        Some(Commands::Cozo { sub }) => {
            let cozo_path = config.cozo_db_path();
            let engine = CozoEngine::open(&cozo_path)?;

            match sub {
                CozoCommands::Query {
                    script,
                    params,
                    json,
                } => {
                    let params_json = if let Some(p) = params {
                        serde_json::from_str(&p)?
                    } else {
                        serde_json::json!({})
                    };
                    let res = engine.datalog_query(&script, params_json)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&res)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&res)?);
                    }
                }
                CozoCommands::Search {
                    query,
                    target,
                    k,
                    json,
                } => {
                    let matches = engine.vector_search(&query, &target, k)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&matches)?);
                    } else {
                        println!(
                            "Vector search results for '{}' (target: {}):",
                            query.bold(),
                            target.cyan()
                        );
                        for (i, m) in matches.iter().enumerate() {
                            let dist_str =
                                format!("score: {:.3}, dist: {:.3}", m.score, m.distance);
                            println!("{}. {} [{}]", i + 1, m.id.bold(), dist_str.dimmed());
                            for (k, v) in &m.metadata {
                                println!("   {}: {}", k.cyan(), v);
                            }
                        }
                    }
                }
                CozoCommands::Pagerank {
                    theta,
                    iterations,
                    json,
                } => {
                    let ranks = engine.pagerank(Some(theta), Some(iterations))?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&ranks)?);
                    } else {
                        println!(
                            "PageRank Centrality Scores (theta: {}, iterations: {}):",
                            theta, iterations
                        );
                        for (i, r) in ranks.iter().take(20).enumerate() {
                            println!("{}. {} - rank: {:.6}", i + 1, r.id.bold(), r.rank);
                        }
                    }
                }
                CozoCommands::ShortestPath {
                    start,
                    target,
                    json,
                } => {
                    let path = engine.shortest_path(&start, &target)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&path)?);
                    } else if let Some(p) = path {
                        println!(
                            "Shortest path from '{}' to '{}' (length {}):",
                            start.bold(),
                            target.bold(),
                            p.len()
                        );
                        for (i, node) in p.iter().enumerate() {
                            if i > 0 {
                                print!(" -> ");
                            }
                            print!("{}", node.cyan());
                        }
                        println!();
                    } else {
                        println!("No path found between '{}' and '{}'", start, target);
                    }
                }
                CozoCommands::Sync { json } => {
                    let mut nodes_synced = 0;
                    let mut edges_synced = 0;
                    let mut wiki_synced = 0;

                    if let Ok(graph_conn) = rusqlite::Connection::open(config.graph_db_path()) {
                        let (n, e) = engine.sync_from_graph(&graph_conn)?;
                        nodes_synced = n;
                        edges_synced = e;
                    }
                    if let Ok(wiki_conn) = rusqlite::Connection::open(config.wiki_db_path()) {
                        wiki_synced = engine.sync_from_wiki(&wiki_conn)?;
                    }

                    if json {
                        let res = serde_json::json!({
                            "status": "ok",
                            "nodes_synced": nodes_synced,
                            "edges_synced": edges_synced,
                            "wiki_synced": wiki_synced,
                            "cozo_db": cozo_path.display().to_string(),
                        });
                        println!("{}", serde_json::to_string_pretty(&res)?);
                    } else {
                        println!(
                            "{} Synchronized SQLite storage to CozoDB:",
                            "[ok]".green().bold()
                        );
                        println!("  - Code Nodes:  {} with 128-dim embeddings", nodes_synced);
                        println!("  - Code Edges:  {}", edges_synced);
                        println!("  - Wiki Pages:  {} with 128-dim embeddings", wiki_synced);
                        println!("  - Storage:     {}", cozo_path.display());
                    }
                }
            }
        }
        Some(Commands::Commands) => {
            println!("{}", "=== Knobyte CLI Commands ===".bold());
            println!("  knobyte                    Launch local Project Hub (browser)");
            println!(
                "  knobyte setup              Initialize Knobyte memory scaffold in repository"
            );
            println!("  knobyte check              Check drift score against code graph");
            println!("  knobyte sync               Plan or fix drifted scaffold files");
            println!("  knobyte graph rebuild      Rebuild tree-sitter code graph into graph.db");
            println!("  knobyte graph query <r> <t> Query code graph (where-defined, who-calls, who-imports)");
            println!("  knobyte graph scope <task> Find relevant code symbols for a coding task");
            println!("  knobyte wiki rebuild-index Rebuild SQLite FTS5 search index into wiki.db");
            println!("  knobyte wiki query <text>  Search wiki knowledge entities");
            println!("  knobyte wiki show <id>     View specific wiki entity");
            println!("  knobyte cozo search <query> HNSW vector similarity search on code & wiki");
            println!("  knobyte cozo query <script> Execute Datalog graph query in CozoDB");
            println!(
                "  knobyte cozo pagerank       Compute PageRank centrality on dependency graph"
            );
            println!("  knobyte cozo shortest-path  Find shortest path between code symbols");
            println!("  knobyte cozo sync           Sync SQLite graph.db and wiki.db to CozoDB");
            println!("  knobyte log <message>      Append decision, discovery, note, risk, todo");
            println!("  knobyte timeline           Search historical project events and notes");
            println!("  knobyte member list        List canonical team members");
            println!("  knobyte relay list         List team handoffs");
            println!("  knobyte relay publish <id> Publish a handoff relay");
            println!("  knobyte inbox draft save   Propose addition/correction to project memory");
            println!("  knobyte heartbeat          Lightweight health and stale file check");
            println!("  knobyte doctor             Full environment and index diagnostic");
            println!("  knobyte skills sync        Sync skills for Claude Code and Codex");
            println!(
                "  knobyte capabilities --json Machine-readable capability discovery for agents"
            );
            println!("  knobyte mcp --sse          Run remote Server-Sent Events (SSE) MCP server");
            println!(
                "  knobyte mcp --stdio        Run stdio MCP server for Cursor / Claude Desktop"
            );
        }
    }

    Ok(())
}
