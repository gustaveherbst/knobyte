use clap::{Parser, Subcommand};
use colored::Colorize;

use knobyte::capabilities::get_capabilities;
use knobyte::config::find_config;
use knobyte::cozo::CozoEngine;
use knobyte::doctor::run_doctor;
use knobyte::heartbeat::run_heartbeat;
use knobyte::hub::start_hub_server;
use knobyte::mcp::{start_sse_server_with_options, start_stdio_server_with, HttpTransport, SseServerOptions};
use knobyte::progress::format_bytes;
use knobyte::team::cli::{
    run_activity, run_catch_up, run_inbox, run_log, run_member, run_playbook, run_relay, run_spec, run_timeline,
    run_workstream, ActivityCommands, CatchUpArgs, InboxCommands, LogArgs, MemberCommands, PlaybookCommands,
    RelayCommands, SpecCommands, TimelineArgs, WorkstreamCommands,
};
use knobyte::wiki::cli::WikiCommands;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_HUB_PORT: u16 = 4000;
const DEFAULT_MCP_PORT: u16 = 3005;

#[derive(Parser)]
#[command(name = "knobyte", version = knobyte::VERSION, about = "Shared project memory, code graphs, and local vector search for engineers and their coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Hub port for bare `knobyte` or `knobyte hub` (same as `knobyte hub --port`)
    #[arg(long, value_parser = parse_port_arg)]
    port: Option<u16>,
    /// Do not open the browser (bare `knobyte` or `knobyte hub`)
    #[arg(long = "no-open")]
    no_open: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the local Project Hub web interface (or setup if unconfigured)
    Hub {
        /// Loopback port to bind (1-65535, default 4000)
        #[arg(long, value_parser = parse_port_arg)]
        port: Option<u16>,
        #[arg(long, default_value = DEFAULT_HOST)]
        host: String,
        #[arg(long = "no-open")]
        no_open: bool,
        /// Access token required by the Hub (exported as KNOBYTE_HUB_TOKEN)
        #[arg(long)]
        token: Option<String>,
    },
    /// CozoDB Datalog graph queries and HNSW vector search
    Cozo {
        #[command(subcommand)]
        sub: CozoCommands,
    },
    /// Set up Knobyte project memory: scaffold, AI tool anchors, skills, graph, population
    Setup {
        /// Show what would happen without making changes
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// code-repo, agent-memory, monorepo or docs-only (default: the saved mode, else code-repo)
        #[arg(long)]
        mode: Option<String>,
        /// Run the interactive terminal flow (tool menu, confirmations)
        #[arg(long)]
        cli: bool,
        /// AI tools to configure: claude, cursor, windsurf, copilot, opencode, codex (comma separated, or none)
        #[arg(long)]
        tools: Option<String>,
        /// Launch Claude Code / Codex to populate the scaffold without asking (explicit consent)
        #[arg(long = "launch-agent", conflicts_with = "no_agent")]
        launch_agent: bool,
        /// Never launch an agent; print the population prompt instead
        #[arg(long = "no-agent")]
        no_agent: bool,
        /// Agent to launch: claude or codex (default: first selected tool that is installed)
        #[arg(long)]
        agent: Option<String>,
        /// Do not build the code graph
        #[arg(long = "skip-graph")]
        skip_graph: bool,
        /// Create the commit checkpoint without asking
        #[arg(long)]
        commit: bool,
        /// Move conflicting skill directories aside instead of stopping
        #[arg(long = "backup-skills")]
        backup_skills: bool,
        /// On an already populated scaffold, also capture missing grounding baselines into the
        /// Markdown (a re-run otherwise leaves tracked files unchanged)
        #[arg(long = "capture-baselines")]
        capture_baselines: bool,
    },
    /// Print a pre-analysed brief of the repository (manifests, entry points, folders, tooling)
    Init {
        #[arg(long)]
        json: bool,
    },
    /// Refresh Knobyte-owned scaffold files and managed blocks without touching populated content
    Update {
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check project memory drift against current codebase
    Check {
        /// Single-line summary only
        #[arg(long)]
        quiet: bool,
        /// Output the full drift report as JSON
        #[arg(long)]
        json: bool,
        /// Rewrite moved grounding references (shows the plan and asks first), then hand
        /// remaining errors to the `sync` repair flow
        #[arg(long)]
        fix: bool,
        /// With --fix: apply the planned changes without asking (required when not interactive
        /// or with --json)
        #[arg(long, short = 'y', requires = "fix")]
        yes: bool,
        /// With --fix: print the planned changes only; write nothing
        #[arg(long = "dry-run", requires = "fix")]
        dry_run: bool,
        /// Show detailed diagnostic output (files scanned, claims, issues per checker)
        #[arg(long)]
        verbose: bool,
        /// Warn when a file hasn't changed in N days (default 30)
        #[arg(long = "stale-warn-days", value_name = "N")]
        stale_warn_days: Option<i64>,
        /// Error when a file hasn't changed in N days (default 90)
        #[arg(long = "stale-error-days", value_name = "N")]
        stale_error_days: Option<i64>,
        /// Warn when a file has N commits since its last change (default 50)
        #[arg(long = "stale-warn-commits", value_name = "N")]
        stale_warn_commits: Option<i64>,
        /// Error when a file has N commits since its last change (default 200)
        #[arg(long = "stale-error-commits", value_name = "N")]
        stale_error_commits: Option<i64>,
    },
    /// Repair drift: relocate moved groundings, then fix files with an agent or printed prompts
    Sync {
        /// Print the repair prompt without launching anything
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Include warning-only files (by default only files with errors)
        #[arg(long)]
        warnings: bool,
        /// Launch Claude Code / Codex without the interactive menu (explicit consent)
        #[arg(long = "launch-agent", conflicts_with = "print_prompt")]
        launch_agent: bool,
        /// Agent to launch: claude or codex
        #[arg(long)]
        agent: Option<String>,
        /// Print the repair prompt to paste into your agent and exit
        #[arg(long = "print-prompt")]
        print_prompt: bool,
        /// Capture grounding baselines for repaired files without asking
        #[arg(long)]
        accept: bool,
        /// Maximum repair cycles
        #[arg(long = "max-cycles", default_value_t = 3)]
        max_cycles: usize,
    },
    /// Build or query the deterministic code graph (bare `knobyte graph` builds it)
    Graph {
        #[command(subcommand)]
        sub: Option<GraphCommands>,
        /// Project root to build (bare `knobyte graph` only; defaults to the discovered project)
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
        /// Resolve TypeScript/JavaScript with the TypeScript type checker (needs Node and a
        /// `typescript` package; falls back to source-only extraction when unavailable)
        #[arg(long = "ts-compiler")]
        ts_compiler: bool,
    },
    /// Find blast radius / impact of a symbol, file or grounding reference
    Impact {
        target: String,
        /// Transitive depth to follow (1-8)
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Follow only call/instantiation edges (transitive callers)
        #[arg(long = "callers-only")]
        callers_only: bool,
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
        /// Agent protocol v3 JSONL (also selected by any budget/detail flag)
        #[arg(long)]
        jsonl: bool,
        #[command(flatten)]
        agent: knobyte::graph::cli_agent::AgentFlags,
    },
    /// Build, query, validate, edit (apply) and synthesize the project Wiki
    Wiki {
        #[command(subcommand)]
        sub: WikiCommands,
    },
    /// Export the whole scaffold as one Markdown bundle (stdout, or --out PATH)
    Export {
        #[arg(long)]
        out: Option<String>,
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
    /// Reusable team playbooks and their runs
    Playbook {
        #[command(subcommand)]
        sub: PlaybookCommands,
    },
    /// What changed in shared memory since you last caught up (mark, reset)
    CatchUp(CatchUpArgs),
    /// Append a note, decision, discovery, risk, or todo to the event log
    Log(LogArgs),
    /// Search recent event log entries and project notes
    Timeline(TimelineArgs),
    /// Health check: stale content docs, workstream consistency, temp-file cleanup
    Heartbeat {
        #[arg(long)]
        json: bool,
        /// Remove orphaned temporary files and stale locks (otherwise only reported)
        #[arg(long)]
        clean: bool,
        /// Days since `last_updated` after which scaffold files are stale (default: heartbeat.staleDays, else 7)
        #[arg(long = "stale-days")]
        stale_days: Option<u64>,
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
    /// Start the Model Context Protocol (MCP) server (HTTP: streamable /mcp and legacy /sse)
    Mcp {
        /// HTTP port (1-65535)
        #[arg(long, default_value_t = DEFAULT_MCP_PORT, value_parser = parse_port_arg)]
        port: u16,
        /// HTTP bind address (non-loopback requires a bearer token)
        #[arg(long, default_value = DEFAULT_HOST)]
        host: String,
        /// Serve only the legacy HTTP+SSE transport (GET /sse + POST /messages)
        #[arg(long, conflicts_with_all = ["stdio", "http"])]
        sse: bool,
        /// Serve only the streamable HTTP transport (POST /mcp)
        #[arg(long, conflicts_with_all = ["stdio", "sse"])]
        http: bool,
        /// Speak MCP over stdin/stdout instead of HTTP (for clients that spawn the server)
        #[arg(long, conflicts_with_all = ["sse", "http"])]
        stdio: bool,
        /// Bearer token required by the MCP server (exported as KNOBYTE_MCP_TOKEN)
        #[arg(long)]
        token: Option<String>,
        /// Tool profile to list (default core; overrides KNOBYTE_MCP_PROFILE and mcp.profile in .knobyte/config.json)
        #[arg(long, value_parser = ["core", "team", "wiki", "graph", "full"])]
        profile: Option<String>,
    },
    /// Create a new pattern template
    Pattern {
        #[command(subcommand)]
        sub: PatternCommands,
    },
    /// Read or set this checkout's advisory agent logging mode
    Logging {
        /// significant (default), checkpoints or manual
        mode: Option<String>,
        /// Require the exact current revision (sha256:...), or `none` for an unset preference
        #[arg(long = "expected-revision")]
        expected_revision: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Install/uninstall a post-commit drift check, or run the heartbeat on an interval
    Watch {
        /// Remove the post-commit hook
        #[arg(long)]
        uninstall: bool,
        /// Run `knobyte heartbeat` every N minutes instead (default: watch.intervalMinutes, else 30)
        #[arg(long, num_args = 0..=1, value_name = "MINUTES", value_parser = clap::value_parser!(u64).range(1..))]
        interval: Option<Option<u64>>,
    },
    /// Print a shell completion script (bash, zsh or fish)
    Completion { shell: String },
    /// Interactive terminal dashboard
    Tui,
    /// Print list of all available commands
    #[command(name = "commands")]
    CommandList,
}

#[derive(Subcommand)]
enum CozoCommands {
    /// Execute a CozoScript Datalog query (read-only unless --mutable)
    Query {
        script: String,
        #[arg(long)]
        params: Option<String>,
        /// Allow the script to modify stored relations (:put, :rm, :create, ...)
        #[arg(long)]
        mutable: bool,
        #[arg(long)]
        json: bool,
    },
    /// HNSW vector similarity search on code nodes or wiki entities
    Search {
        query: String,
        /// What to search: code nodes or wiki entities
        #[arg(long, default_value = "code", value_parser = ["code", "wiki"])]
        target: String,
        /// Number of matches to return (fewer only when fewer candidates clear the floor)
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Relevance floor, 0-1 (score = 1 - cosine distance; default 0.20, 0 disables it)
        #[arg(long = "min-score", value_name = "SCORE", value_parser = parse_score_arg)]
        min_score: Option<f64>,
        /// Print the matches as a JSON array (floor notes go to stderr)
        #[arg(long)]
        json: bool,
    },
    /// Compute PageRank centrality scores on code dependency graph
    Pagerank {
        /// Damping factor (alias: --theta)
        #[arg(long = "damping", alias = "theta", default_value_t = 0.85)]
        damping: f64,
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
    /// Manage the embedding backend and local embedding models
    Model {
        #[command(subcommand)]
        sub: CozoModelCommands,
    },
}

#[derive(Subcommand)]
enum CozoModelCommands {
    /// Download a Model2Vec model from Hugging Face into ~/.knobyte/models (explicit only)
    Pull {
        /// Hugging Face repo id
        #[arg(long, default_value = knobyte::config::DEFAULT_EMBEDDING_MODEL)]
        model: String,
        /// Re-download files that are already present
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show the active embedding backend, model path, dimension and index state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Select the embedding backend (hashed | model2vec), saved in .knobyte/config.json
    Use {
        backend: String,
        /// Hugging Face repo id of the Model2Vec model (model2vec only)
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum GraphCommands {
    /// Read-only health: fresh, stale, degraded, corrupt, rebuild_required or missing
    Status {
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Incremental refresh: re-extract only changed files, publish atomically
    Refresh {
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
        /// Resolve TypeScript/JavaScript with the TypeScript type checker (needs Node and a
        /// `typescript` package; falls back to source-only extraction when unavailable)
        #[arg(long = "ts-compiler")]
        ts_compiler: bool,
        /// Wait up to this many seconds for a concurrent rebuild / refresh / repair
        #[arg(long = "lock-timeout", value_name = "SECONDS")]
        lock_timeout: Option<u64>,
    },
    /// Full rebuild into an isolated candidate, published atomically
    Rebuild {
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
        /// Resolve TypeScript/JavaScript with the TypeScript type checker (needs Node and a
        /// `typescript` package; falls back to source-only extraction when unavailable)
        #[arg(long = "ts-compiler")]
        ts_compiler: bool,
        /// Wait up to this many seconds for a concurrent rebuild / refresh / repair
        #[arg(long = "lock-timeout", value_name = "SECONDS")]
        lock_timeout: Option<u64>,
    },
    /// Structural lookup: where-defined, who-calls, what-calls, who-imports
    Query {
        relation: String,
        target: String,
        #[arg(long)]
        json: bool,
        /// Agent protocol v3 JSONL (also selected by any budget/detail flag)
        #[arg(long)]
        jsonl: bool,
        #[command(flatten)]
        agent: knobyte::graph::cli_agent::AgentFlags,
    },
    /// Task-scoped retrieval: ranked files, source, directed flows and facts in one response
    Scope {
        tasks: Vec<String>,
        /// Protocol records as a JSON array
        #[arg(long)]
        json: bool,
        /// Agent protocol v3 JSONL
        #[arg(long)]
        jsonl: bool,
        /// Attach wiki entities grounded to the returned nodes
        #[arg(long)]
        wiki: bool,
        /// Re-rank with Cozo vector similarity (optional; falls back to lexical ranking)
        #[arg(long)]
        hybrid: bool,
        #[command(flatten)]
        agent: knobyte::graph::cli_agent::AgentFlags,
    },
    /// Nodes by id or grounding reference, optionally with their source
    Get {
        ids: Vec<String>,
        /// Include each node's source lines
        #[arg(long)]
        source: bool,
        /// Maximum source lines per node (implies --source; capped at 400)
        #[arg(long = "max-lines")]
        max_lines: Option<usize>,
        #[arg(long)]
        json: bool,
        /// Agent protocol v3 JSONL (also selected by any budget/detail flag)
        #[arg(long)]
        jsonl: bool,
        #[command(flatten)]
        agent: knobyte::graph::cli_agent::AgentFlags,
    },
    /// Grounding baselines and retro-grounding (default: re-baseline grounded references)
    Ground {
        /// Re-baseline grounded references to the current code (the default mode)
        #[arg(long)]
        rebaseline: bool,
        /// Propose groundings for wiki entities that have none, without writing
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Write proposed groundings into the documents, then re-baseline
        #[arg(long)]
        apply: bool,
        /// Agent-led retro-grounding (with --dry-run: print the agent prompt). Shows the
        /// command and asks before launching unless --launch-agent is given
        #[arg(long)]
        agent: bool,
        /// With --agent: launch without asking (still disabled by KNOBYTE_NO_AGENT_LAUNCH)
        #[arg(long = "launch-agent", requires = "agent")]
        launch_agent: bool,
        /// Maximum proposed references per entity
        #[arg(long = "per-entity", default_value_t = 3)]
        per_entity: usize,
        #[arg(long)]
        json: bool,
    },
    /// Repair the graph store in place (WAL recovery, index/FTS rebuild, schema upgrade, dangling rows)
    Repair {
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
        /// Wait up to this many seconds for a concurrent rebuild / refresh / repair
        #[arg(long = "lock-timeout", value_name = "SECONDS")]
        lock_timeout: Option<u64>,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    /// Install or update the official skills and managed instruction blocks (never clobbers edits)
    Sync {
        /// claude, codex or all
        #[arg(long)]
        tool: Option<String>,
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Move conflicting skill directories to .knobyte/local/skill-backups/ and reinstall
        #[arg(long)]
        backup: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PatternCommands {
    /// Create patterns/<name>.md (never overwrites) and list it in patterns/INDEX.md
    Add { name: String },
}

/// Writing to a closed stdout/stderr (e.g. `knobyte ... | head`) makes `println!` panic with
/// "failed printing to stdout: Broken pipe". Treat that like a CLI killed by SIGPIPE: exit
/// quietly instead of printing a panic. Other panics keep the default report. (SIGPIPE itself
/// stays ignored so the Hub, MCP server and agent child pipes still see EPIPE as an error.)
fn install_broken_pipe_handler() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("");
        if msg.starts_with("failed printing to std") && is_broken_pipe_message(msg) {
            exit_broken_pipe();
        }
        default_hook(info);
    }));
}

fn is_broken_pipe_message(msg: &str) -> bool {
    msg.contains("Broken pipe") || msg.contains("os error 32") || msg.contains("os error 232")
}

fn exit_broken_pipe() -> ! {
    #[cfg(unix)]
    unsafe {
        // Re-raise with the default action so the shell sees the usual SIGPIPE status.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::raise(libc::SIGPIPE);
    }
    std::process::exit(141)
}

/// Commands that may run outside a set-up Knobyte project. Everything else refuses (and
/// creates nothing) unless `.knobyte/` holds a complete scaffold.
fn runs_without_project(command: &Option<Commands>) -> bool {
    match command {
        None
        | Some(Commands::Hub { .. })
        | Some(Commands::Setup { .. })
        | Some(Commands::Init { .. })
        | Some(Commands::Completion { .. })
        | Some(Commands::CommandList)
        | Some(Commands::Capabilities { .. })
        | Some(Commands::Mcp { .. })
        // The code graph indexes any directory (`--root`).
        | Some(Commands::Graph { .. })
        | Some(Commands::Impact { .. }) => true,
        Some(Commands::Cozo { sub: CozoCommands::Model { sub } }) => {
            matches!(sub, CozoModelCommands::Pull { .. } | CozoModelCommands::Status { .. })
        }
        _ => false,
    }
}

/// Refuse a command that needs a scaffold when there is none. JSON callers also get a
/// machine-readable error on stdout (`SKILL_SYNC_FAILED` for `skills sync`).
fn refuse_without_project(command: &Option<Commands>, problem: &knobyte::config::ProjectProblem) -> ! {
    let json = std::env::args().any(|a| a == "--json");
    if json {
        let code = match command {
            Some(Commands::Skills { .. }) => "SKILL_SYNC_FAILED".to_string(),
            _ => problem.code().to_ascii_uppercase(),
        };
        println!(
            "{}",
            serde_json::json!({ "schemaVersion": 1, "ok": false, "error": { "code": code, "reason": problem.code(), "message": problem.to_string() } })
        );
    }
    eprintln!("{} {}", "[error]".red().bold(), problem);
    // Exit code 3: repository state unavailable (see `knobyte capabilities --json`).
    std::process::exit(3);
}

/// Port values: 1-65535. Port 0 (an OS-chosen ephemeral port) is
/// rejected because the printed Hub link would carry `:0` instead of the bound port.
fn parse_port_arg(raw: &str) -> Result<u16, String> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("Expected a positive integer, got \"{raw}\"."));
    }
    match raw.parse::<u64>() {
        Ok(0) => Err(format!("Expected a positive integer, got \"{raw}\".")),
        Ok(n) if n <= 65_535 => Ok(n as u16),
        _ => Err(format!("Expected a TCP port from 1 to 65535, got \"{raw}\".")),
    }
}

/// Print the `check --fix` plan: every anchor rewrite, grouped by scaffold file.
fn print_fix_plan(proposals: &[knobyte::drift::RelocationProposal], dry_run: bool) {
    if proposals.is_empty() {
        println!("{} No moved grounding references to rewrite.", "[info]".cyan().bold());
        return;
    }
    println!(
        "{}",
        format!(
            "Planned grounding changes{} ({}):",
            if dry_run { " (dry run, nothing is written)" } else { "" },
            proposals.len()
        )
        .bold()
    );
    let mut files: Vec<&str> = Vec::new();
    for p in proposals {
        if !files.contains(&p.scaffold_file.as_str()) {
            files.push(&p.scaffold_file);
        }
    }
    for file in files {
        println!("  {}", file.cyan());
        for p in proposals.iter().filter(|p| p.scaffold_file == file) {
            println!(
                "    {} -> {}  {}",
                p.old_node_id.red(),
                p.new_node_id.green(),
                format!("({:.0}% confidence)", p.confidence * 100.0).dimmed()
            );
        }
    }
    println!();
}

/// A relevance score from 0 to 1 (`cozo search --min-score`).
fn parse_score_arg(raw: &str) -> Result<f64, String> {
    match raw.trim().parse::<f64>() {
        Ok(v) if (0.0..=1.0).contains(&v) => Ok(v),
        _ => Err(format!("Expected a score from 0 to 1, got \"{raw}\".")),
    }
}

#[tokio::main]
async fn main() {
    install_broken_pipe_handler();
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        // Display, never Debug: `Error: "Refusing…"` is not a user-facing message.
        eprintln!("{} {}", "[error]".red().bold(), e);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // The global --port/--no-open also apply to `knobyte hub`.
    let global_hub_opts = cli.port.is_some() || cli.no_open;
    if global_hub_opts && cli.command.is_some() && !matches!(cli.command, Some(Commands::Hub { .. })) {
        eprintln!(
            "{} --port and --no-open apply to bare `knobyte` and `knobyte hub` only; use `knobyte hub --port N --no-open`.",
            "[error]".red().bold()
        );
        std::process::exit(2);
    }
    let config = find_config(None).unwrap_or_else(|_| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        knobyte::config::KnobyteConfig::new(cwd.clone(), cwd.join(".knobyte"))
    });
    if !runs_without_project(&cli.command) {
        if let Err(problem) = knobyte::config::require_project(&config) {
            refuse_without_project(&cli.command, &problem);
        }
    }

    match cli.command {
        None => {
            if !config.scaffold_root.exists() {
                println!(
                    "{}",
                    "No Knobyte scaffold found. The Hub opens on its Setup page.".yellow()
                );
            }
            start_hub_server(config, DEFAULT_HOST, cli.port.unwrap_or(DEFAULT_HUB_PORT), !cli.no_open).await?;
        }
        Some(Commands::Hub {
            port,
            host,
            no_open,
            token,
        }) => {
            if let Some(t) = token {
                std::env::set_var("KNOBYTE_HUB_TOKEN", t);
            }
            if !config.scaffold_root.exists() {
                println!(
                    "{}",
                    "No Knobyte scaffold found. The Hub opens on its Setup page.".yellow()
                );
            }
            let port = port.or(cli.port).unwrap_or(DEFAULT_HUB_PORT);
            let no_open = no_open || cli.no_open;
            start_hub_server(config, &host, port, !no_open).await?;
        }
        Some(Commands::Setup {
            dry_run,
            mode,
            cli,
            tools,
            launch_agent,
            no_agent,
            agent,
            skip_graph,
            commit,
            backup_skills,
            capture_baselines,
        }) => {
            use knobyte::setup::flow::{parse_tool_list, run_setup_flow, SetupFlowOptions};
            let tools = match tools.as_deref().map(parse_tool_list).transpose() {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("{} {}", "[error]".red().bold(), e);
                    std::process::exit(2);
                }
            };
            let opts = SetupFlowOptions {
                mode,
                dry_run,
                tools,
                interactive: cli || knobyte::agent::is_interactive(),
                launch_agent,
                no_agent,
                agent,
                skip_graph,
                commit,
                backup_skills,
                capture_baselines,
            };
            match run_setup_flow(&config, &opts) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{} {}", "[error]".red().bold(), e);
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Init { json }) => {
            let brief = knobyte::scanner::scan(&config.project_root);
            if json {
                println!("{}", serde_json::to_string_pretty(&brief)?);
            } else {
                println!("{}", knobyte::scanner::build_prompt(&brief));
            }
        }
        Some(Commands::Update { dry_run, json }) => match knobyte::setup::update::run_update(&config, dry_run) {
            Ok(report) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    if report.changes.is_empty() {
                        println!("{} Knobyte files are up to date.", "[ok]".green().bold());
                    }
                    for c in &report.changes {
                        let tag = if c.action == "conflict" { "[warn]".yellow().bold() } else { "[ok]".green().bold() };
                        println!(
                            "{} {}{} {} ({})",
                            tag,
                            if dry_run && c.action != "conflict" { "would " } else { "" },
                            c.action,
                            c.path,
                            c.detail
                        );
                    }
                    println!("{}", "Populated content was not modified.".dimmed());
                }
                if report.conflicts > 0 {
                    std::process::exit(4);
                }
            }
            Err(e) => {
                eprintln!("{} {}", "[error]".red().bold(), e);
                std::process::exit(3);
            }
        },
        Some(Commands::Check {
            quiet,
            json,
            fix,
            yes,
            dry_run,
            verbose,
            stale_warn_days,
            stale_error_days,
            stale_warn_commits,
            stale_error_commits,
        }) => {
            // `--fix` writes, so it plans first: the exact anchor rewrites are printed, then
            // applied only with consent (interactive confirmation, or --yes).
            let mut fix_summary: Option<serde_json::Value> = None;
            let mut fix_refused = false;
            let mut fix_applied = false;
            let mut fix_declined = false;
            if fix {
                let plan = knobyte::drift::sync_groundings(&config, true)?;
                let proposals = &plan.proposals;
                if let (Some(reason), false) = (&plan.skipped, json) {
                    println!(
                        "{} Grounding relocation skipped: the code graph is not fresh ({}).",
                        "[info]".cyan().bold(),
                        reason
                    );
                }
                let show_plan = !json && (!quiet || !proposals.is_empty());
                if show_plan {
                    print_fix_plan(proposals, dry_run);
                }
                let interactive = knobyte::agent::is_interactive();
                let apply = !proposals.is_empty()
                    && !dry_run
                    && (yes
                        || (!json
                            && interactive
                            && knobyte::agent::confirm(
                                &format!("Apply these {} grounding change(s)?", proposals.len()),
                                true,
                            )));
                if apply {
                    let n = knobyte::drift::apply_grounding_relocations(&config, proposals)?;
                    fix_applied = true;
                    if !json {
                        println!("{} Relocated {} grounding anchor(s).\n", "[ok]".green().bold(), n);
                    }
                } else if !proposals.is_empty() && !dry_run {
                    if json || !interactive {
                        fix_refused = true;
                        if !json {
                            eprintln!(
                                "{} Not writing: --fix needs --yes to apply changes {}. Re-run with `knobyte check --fix --yes`, or preview with --dry-run.",
                                "[error]".red().bold(),
                                if json { "with --json" } else { "in a non-interactive session" }
                            );
                            std::process::exit(5);
                        }
                    } else {
                        fix_declined = true;
                        println!("{}\n", "No files were changed.".dimmed());
                    }
                }
                fix_summary = Some(serde_json::json!({
                    "dryRun": dry_run,
                    "applied": fix_applied,
                    "skipped": plan.skipped,
                    "planned": proposals.iter().map(|p| serde_json::json!({
                        "file": p.scaffold_file,
                        "oldRef": p.old_node_id,
                        "newRef": p.new_node_id,
                        "confidence": p.confidence,
                        "reason": p.reason,
                    })).collect::<Vec<_>>(),
                }));
            }
            let base = knobyte::config::DriftSettings::load(&config.scaffold_root)
                .staleness_thresholds;
            let staleness = knobyte::config::StalenessThresholds {
                warn_days: stale_warn_days.unwrap_or(base.warn_days),
                error_days: stale_error_days.unwrap_or(base.error_days),
                warn_commits: stale_warn_commits.unwrap_or(base.warn_commits),
                error_commits: stale_error_commits.unwrap_or(base.error_commits),
            };
            let report = knobyte::drift::run_drift_check_with(
                &config,
                &knobyte::drift::DriftCheckOptions {
                    verbose,
                    staleness: Some(staleness),
                    scaffold_patterns: None,
                },
            );
            let errors = report.count("error");
            let warnings = report.count("warning");
            let infos = report.count("info");
            if json {
                let mut value = serde_json::to_value(&report)?;
                if let (Some(obj), Some(fix)) = (value.as_object_mut(), fix_summary) {
                    obj.insert("fix".to_string(), fix);
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if quiet {
                let mut parts = Vec::new();
                if errors > 0 {
                    parts.push(format!("{} error{}", errors, if errors > 1 { "s" } else { "" }));
                }
                if warnings > 0 {
                    parts.push(format!(
                        "{} warning{}",
                        warnings,
                        if warnings > 1 { "s" } else { "" }
                    ));
                }
                let detail = if parts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", parts.join(", "))
                };
                let graph = report
                    .graph
                    .as_ref()
                    .map(|g| format!(" · {}", g.summary()))
                    .unwrap_or_default();
                println!("knobyte: drift score {}/100{}{}", report.score, detail, graph);
            } else {
                let color_score = |s: f64| {
                    let text = format!("{}/100", s);
                    if s >= 80.0 {
                        text.green()
                    } else if s >= 50.0 {
                        text.yellow()
                    } else {
                        text.red()
                    }
                };
                if verbose {
                    if let Some(log) = &report.verbose_log {
                        println!("{}", "── Verbose ──".dimmed());
                        for line in log {
                            println!("  {}", line.dimmed());
                        }
                        println!();
                    }
                }
                for severity in knobyte::drift::checker::SEVERITY_ORDER {
                    let of_sev: Vec<&knobyte::drift::DriftIssue> = report
                        .issues
                        .iter()
                        .filter(|i| i.severity == *severity)
                        .collect();
                    if of_sev.is_empty() {
                        continue;
                    }
                    println!("{}\n", severity.to_uppercase().bold());
                    let mut files: Vec<&str> = Vec::new();
                    for i in &of_sev {
                        if !files.contains(&i.file.as_str()) {
                            files.push(&i.file);
                        }
                    }
                    for file in files {
                        println!("{}", file.bold().underline());
                        for issue in of_sev.iter().filter(|i| i.file == file) {
                            let label = match *severity {
                                "error" => format!("x {}", issue.code).red(),
                                "warning" => format!("! {}", issue.code).yellow(),
                                _ => format!("i {}", issue.code).blue(),
                            };
                            let loc = issue.line.map(|l| format!(":{}", l)).unwrap_or_default();
                            println!("  {}{} {}", label, loc, issue.message);
                            if let Some(hint) = knobyte::drift::checker::remediation_for(&issue.code) {
                                println!("    {}", format!("-> {}", hint).dimmed());
                            }
                        }
                        println!();
                    }
                }
                println!(
                    "{}",
                    format!(
                        "Drift score: {} — {} errors, {} warnings, {} info",
                        color_score(report.score),
                        errors,
                        warnings,
                        infos
                    )
                    .bold()
                );
                println!("{}", format!("{} files checked", report.file_count).dimmed());
                let g = &report.grounding;
                if g.total > 0 {
                    println!(
                        "Groundings: {:.1}% intact ({} intact, {} changed, {} moved, {} ambiguous, {} gone, {} unverified of {})",
                        report.grounding_score,
                        g.intact,
                        g.changed,
                        g.moved,
                        g.ambiguous,
                        g.gone,
                        g.unverified,
                        g.total
                    );
                }
                if let Some(graph) = &report.graph {
                    println!("{}", graph.summary().dimmed());
                }
                for nudge in &report.nudges {
                    println!("{} {}", "[note]".cyan().bold(), nudge);
                }
                if report.issues.is_empty() {
                    println!(
                        "{} All scaffold files pristine and in sync with codebase.",
                        "[ok]".green().bold()
                    );
                }
            }
            if fix_refused {
                // --json without --yes: the plan is in the report; nothing was written.
                std::process::exit(5);
            }
            if dry_run {
                if errors > 0 && !json {
                    println!(
                        "{} Without --dry-run, the {} remaining error(s) would then go to the `knobyte sync` repair flow (it asks before launching an agent).",
                        "[info]".cyan().bold(),
                        errors
                    );
                }
                // Like `sync --dry-run`: a preview reports, it does not gate.
                return Ok(());
            }
            // Errors remain after relocation: hand them to the sync repair flow, which
            // respects the agent-launch rules (non-interactive: prints the repair brief).
            // Relocation already happened above (or was declined), so the flow skips it.
            // JSON output stays machine-readable, so it only reports and exits non-zero.
            if fix && errors > 0 && !json && !fix_declined {
                println!();
                let code = knobyte::agent::sync::run_sync(
                    &config,
                    &knobyte::agent::sync::SyncOptions { skip_relocation: true, ..Default::default() },
                )
                .unwrap_or_else(|e| {
                    eprintln!("{} {}", "[error]".red().bold(), e);
                    1
                });
                exit_on_failure(code);
                return Ok(());
            }
            if errors > 0 {
                std::process::exit(1);
            }
        }
        Some(Commands::Sync {
            dry_run,
            warnings,
            launch_agent,
            agent,
            print_prompt,
            accept,
            max_cycles,
        }) => {
            let opts = knobyte::agent::sync::SyncOptions {
                dry_run,
                include_warnings: warnings,
                launch_agent,
                agent,
                print_prompt,
                accept,
                max_cycles: Some(max_cycles),
                skip_relocation: false,
            };
            match knobyte::agent::sync::run_sync(&config, &opts) {
                Ok(code) => exit_on_failure(code),
                Err(e) => {
                    eprintln!("{} {}", "[error]".red().bold(), e);
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Graph { sub, root, json, ts_compiler }) => {
            use knobyte::graph::cli as graph_cli;
            if ts_compiler {
                knobyte::graph::ts_compiler::request_for_process();
            }
            let exit = |code: i32| {
                if code != 0 {
                    std::process::exit(code);
                }
            };
            let sub = match sub {
                None => {
                    let cfg = graph_cli::config_for_root(&config, root.as_deref());
                    exit(graph_cli::run_build(&cfg, false, json));
                    return Ok(());
                }
                Some(sub) => sub,
            };
            match sub {
                GraphCommands::Status { root, json } => {
                    let cfg = graph_cli::config_for_root(&config, root.as_deref());
                    exit(graph_cli::run_status(&cfg, json));
                }
                GraphCommands::Refresh { root, json, lock_timeout, ts_compiler } => {
                    if ts_compiler {
                        knobyte::graph::ts_compiler::request_for_process();
                    }
                    let cfg = graph_cli::config_for_root(&config, root.as_deref());
                    let opts = graph_cli::cli_maintenance_options(lock_timeout);
                    exit(graph_cli::run_build_with(&cfg, true, json, &opts));
                }
                GraphCommands::Rebuild { root, json, lock_timeout, ts_compiler } => {
                    if ts_compiler {
                        knobyte::graph::ts_compiler::request_for_process();
                    }
                    let cfg = graph_cli::config_for_root(&config, root.as_deref());
                    let opts = graph_cli::cli_maintenance_options(lock_timeout);
                    exit(graph_cli::run_build_with(&cfg, false, json, &opts));
                }
                GraphCommands::Repair { root, json, lock_timeout } => {
                    let cfg = graph_cli::config_for_root(&config, root.as_deref());
                    let opts = graph_cli::cli_maintenance_options(lock_timeout);
                    exit(graph_cli::run_repair_with(&cfg, json, &opts));
                }
                GraphCommands::Get {
                    ids,
                    source,
                    max_lines,
                    json,
                    jsonl,
                    mut agent,
                } => {
                    if jsonl || agent.any_set() {
                        if agent.max_source_lines.is_none() {
                            agent.max_source_lines = max_lines;
                        }
                        exit(knobyte::graph::cli_agent::run_get_protocol(&config, &ids, &agent)?);
                        return Ok(());
                    }
                    let source = source || max_lines.is_some();
                    exit(graph_cli::run_get(&config, &ids, source, max_lines, json, jsonl)?);
                }
                GraphCommands::Query {
                    relation,
                    target,
                    json,
                    jsonl,
                    agent,
                } => {
                    if jsonl || agent.any_set() {
                        exit(knobyte::graph::cli_agent::run_query_protocol(&config, &relation, &target, &agent)?);
                        return Ok(());
                    }
                    exit(graph_cli::run_query(&config, &relation, &target, json, jsonl)?);
                }
                GraphCommands::Scope {
                    tasks,
                    json,
                    jsonl,
                    wiki,
                    hybrid,
                    agent,
                } => {
                    let task_str = tasks.join(" ");
                    exit(knobyte::graph::cli_agent::run_scope_cmd(
                        &config, &task_str, &agent, wiki, hybrid, json, jsonl,
                    )?);
                }
                GraphCommands::Ground {
                    rebaseline,
                    dry_run,
                    apply,
                    agent,
                    launch_agent,
                    per_entity,
                    json,
                } => {
                    let mode = knobyte::graph::cli_agent::GroundMode {
                        rebaseline,
                        dry_run,
                        apply,
                        agent,
                        launch_agent,
                        per_entity,
                        json,
                    };
                    exit(knobyte::graph::cli_agent::run_ground(&config, &mode)?);
                }
            }
        }
        Some(Commands::Impact {
            target,
            depth,
            callers_only,
            root,
            json,
            jsonl,
            agent,
        }) => {
            let cfg = knobyte::graph::cli::config_for_root(&config, root.as_deref());
            let opts = knobyte::graph::ImpactOptions {
                depth,
                callers_only,
            };
            let code = if jsonl || agent.any_set() {
                knobyte::graph::cli_agent::run_impact_protocol(&cfg, &target, opts, &agent)?
            } else {
                knobyte::graph::cli::run_impact(&cfg, &target, opts, json, jsonl)?
            };
            if code != 0 {
                std::process::exit(code);
            }
        }
        Some(Commands::Wiki { sub }) => knobyte::wiki::cli::run_wiki_command(&config, sub)?,
        Some(Commands::Export { out }) => knobyte::wiki::cli::run_export(&config, out)?,
        Some(Commands::Member { sub }) => exit_on_failure(run_member(&config, sub)),
        Some(Commands::Activity { sub }) => exit_on_failure(run_activity(&config, sub)),
        Some(Commands::Workstream { sub }) => exit_on_failure(run_workstream(&config, sub)),
        Some(Commands::Spec { sub }) => exit_on_failure(run_spec(&config, sub)),
        Some(Commands::Inbox { sub }) => exit_on_failure(run_inbox(&config, sub)),
        Some(Commands::Relay { sub }) => exit_on_failure(run_relay(&config, sub)),
        Some(Commands::Playbook { sub }) => exit_on_failure(run_playbook(&config, sub)),
        Some(Commands::CatchUp(args)) => exit_on_failure(run_catch_up(&config, args)),
        Some(Commands::Log(args)) => exit_on_failure(run_log(&config, args)),
        Some(Commands::Timeline(args)) => exit_on_failure(run_timeline(&config, args)),
        Some(Commands::Heartbeat {
            json,
            clean,
            stale_days,
        }) => {
            let stale_days = stale_days.unwrap_or_else(|| knobyte::heartbeat::configured_stale_days(&config));
            let report = run_heartbeat(&config, stale_days, clean);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                if !report.scaffold_exists {
                    println!(
                        "{} No Knobyte scaffold found. Run 'knobyte setup'.",
                        "[warn]".yellow().bold()
                    );
                } else if report.heartbeat_ok {
                    println!("HEARTBEAT_OK");
                } else {
                    println!("{}", "Heartbeat needs attention".bold());
                    if let Some(w) = &report.uncommitted_steps_warning {
                        println!("{} {}", "[warn]".yellow().bold(), w);
                    }
                }
                if report.files_without_last_updated > 0 {
                    println!(
                        "{}",
                        format!(
                            "{} scaffold file(s) have no parseable last_updated; their file age is used instead. Add `last_updated: YYYY-MM-DD` to frontmatter.",
                            report.files_without_last_updated
                        )
                        .dimmed()
                    );
                }
                if !report.stale_files.is_empty() {
                    println!("\n{}", format!("Stale scaffold files (older than {} days):", stale_days).yellow());
                    for sf in &report.stale_files {
                        println!("  - {} ({} days, {})", sf.path, sf.age_days, sf.source);
                    }
                    println!("{}", "  Review these files and run `knobyte sync` if they no longer match reality.".dimmed());
                }
                if report.memory_cleanup_due {
                    println!("\n{}", "Memory cleanup is due.".yellow());
                    println!("{}", "  Review the daily memory files and promote durable details to long-term memory.".dimmed());
                }
                if !report.old_daily_memory_files.is_empty() {
                    println!("\n{}", "Old daily memory files:".yellow());
                    for m in &report.old_daily_memory_files {
                        println!("  - {}", m);
                    }
                }
                if !report.cleaned.is_empty() {
                    println!("\nRemoved temporary files:");
                    for p in &report.cleaned {
                        println!("  - {}", p);
                    }
                }
                let pending: Vec<_> = report
                    .cleanup_candidates
                    .iter()
                    .filter(|c| !report.cleaned.contains(&c.path))
                    .collect();
                if !pending.is_empty() {
                    println!(
                        "\nTemporary files that would be removed{}:",
                        if clean { " (removal failed)" } else { " (run with --clean)" }
                    );
                    for c in pending {
                        println!("  - {} ({})", c.path, c.reason);
                    }
                }
            }
        }
        Some(Commands::Doctor { json }) => {
            let doc = run_doctor(&config);
            if json {
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                let line = |ok: bool, label: &str, detail: String| {
                    let icon = if ok { "ok".green().bold() } else { " !".yellow().bold() };
                    println!("{} {:<11} {}", icon, label.bold(), detail.dimmed());
                };
                println!("{}", "knobyte doctor".bold());
                println!("{}", format!("Scaffold: {}", config.scaffold_root.display()).dimmed());
                println!();
                line(
                    doc.drift_score >= 80.0 && doc.drift_errors == 0,
                    "Drift",
                    format!("{:.0}/100 ({} errors, {} warnings)", doc.drift_score, doc.drift_errors, doc.drift_warnings),
                );
                line(doc.graph_status == "fresh", "Graph", format!("{}; {}", doc.graph_status, doc.graph_detail));
                match &doc.coverage {
                    Some(c) if c.unindexed_total == 0 && !c.truncated => {
                        line(true, "Coverage", "all recognized source files are indexable".to_string())
                    }
                    Some(c) => line(
                        false,
                        "Coverage",
                        format!(
                            "{} source file(s) not indexable by any extractor: {}{}",
                            c.unindexed_total,
                            c.unindexed.join(", "),
                            if c.truncated { ", walk stopped early" } else { "" }
                        ),
                    ),
                    None => line(false, "Coverage", "unknown (no coverage recorded); run knobyte graph rebuild".to_string()),
                }
                line(doc.heartbeat_ok, "Heartbeat", doc.heartbeat_detail.clone());
                line(true, "Events", format!("{} logged event{}", doc.event_count, if doc.event_count == 1 { "" } else { "s" }));
                line(
                    doc.wiki_db_ready,
                    "Wiki",
                    if doc.wiki_db_ready { format!("ready ({} entities)", doc.wiki_entities) } else { "index not built".to_string() },
                );
                line(
                    true,
                    "CozoDB",
                    if doc.cozodb_ready { "ready (Datalog + HNSW vector)".to_string() } else { "not initialized".to_string() },
                );
                let emb = &doc.embedding;
                let dim = emb.dim.map(|d| format!("{}-dim", d)).unwrap_or_else(|| "dim unknown".to_string());
                match &emb.model {
                    Some(model) => line(
                        emb.model_present,
                        "Embeddings",
                        format!("{} {} ({}, {})", emb.backend, model, dim, if emb.model_present { "model present" } else { "model missing" }),
                    ),
                    None => line(true, "Embeddings", format!("{} ({})", emb.backend, dim)),
                }
                line(
                    doc.scaffold_configured,
                    "Config",
                    if doc.scaffold_configured {
                        format!("config.json loaded (mode: {}, git: {})", doc.mode, if doc.git_repository { "yes" } else { "no" })
                    } else {
                        "no config.json; using defaults".to_string()
                    },
                );
                if !doc.next_steps.is_empty() {
                    println!("\n{}", "Next steps".bold());
                    for step in &doc.next_steps {
                        println!("  {}", step);
                    }
                }
            }
            exit_on_failure(doc.exit_code);
        }
        Some(Commands::Skills { sub }) => match sub {
            SkillCommands::Sync {
                tool,
                dry_run,
                backup,
                json,
            } => {
                let opts = knobyte::skills::SkillSyncOptions { dry_run, check_ignored: true, backup_conflicts: backup };
                let rep = match knobyte::skills::sync_skills_with(&config, tool.as_deref(), opts) {
                    Ok(rep) => rep,
                    Err(message) => {
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({ "schemaVersion": 1, "ok": false, "error": { "code": "SKILL_SYNC_FAILED", "message": message } })
                            );
                        } else {
                            eprintln!("{} {}", "[error]".red().bold(), message);
                        }
                        std::process::exit(1);
                    }
                };
                if json {
                    let mut v = serde_json::json!({
                        "schemaVersion": 1,
                        "ok": !rep.conflicted,
                        "packageVersion": knobyte::VERSION,
                    });
                    if let (Some(env), serde_json::Value::Object(body)) = (v.as_object_mut(), serde_json::to_value(&rep)?) {
                        env.extend(body);
                    }
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    for act in &rep.actions {
                        if act.action == "conflict" {
                            continue;
                        }
                        let verb = if rep.dry_run && act.action != "unchanged" {
                            format!("would {}", act.action)
                        } else {
                            act.action.clone()
                        };
                        println!(
                            "{} {} [{}] {} -> {}",
                            "[ok]".green().bold(),
                            act.client,
                            act.skill_name,
                            verb,
                            act.path
                        );
                    }
                    for w in &rep.warnings {
                        println!("{} {}", "[warn]".yellow().bold(), w.message);
                        if let Some(r) = &w.resolution {
                            println!("{}", r.dimmed());
                        }
                    }
                }
                if rep.conflicted {
                    std::process::exit(4);
                }
            }
        },
        Some(Commands::Capabilities { json }) => {
            let caps = get_capabilities(&config);
            if json {
                println!("{}", serde_json::to_string_pretty(&caps)?);
            } else {
                println!("Knobyte {} capabilities (schema v{})", caps.knobyte_version, caps.schema_version);
                println!(
                    "Repository: {} (graph: {}, wiki: {})",
                    caps.repository.initialization_state, caps.repository.graph_index_state, caps.repository.wiki_index_state
                );
                if let Some(next) = &caps.next_initialization_action {
                    println!("Next: {}{}", next.command.as_deref().map(|c| format!("{} — ", c)).unwrap_or_default(), next.reason);
                }
                for c in &caps.capabilities {
                    println!("  - {}: {}", c.id, c.availability);
                }
                println!("{} commands; run with --json for descriptors and exit codes.", caps.commands.len());
            }
        }
        Some(Commands::Mcp {
            port,
            host,
            sse,
            http,
            stdio,
            token,
            profile,
        }) => {
            if let Some(t) = token {
                std::env::set_var("KNOBYTE_MCP_TOKEN", t);
            }
            let scaffold_root = knobyte::mcp::handler::server_config().scaffold_root;
            let resolved = match knobyte::mcp::resolve_profile_for(profile.as_deref(), &scaffold_root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{} {}", "[error]".red().bold(), e);
                    std::process::exit(2);
                }
            };
            let profile_line = format!(
                "tool profile {} ({} tools, from {})",
                resolved.profile,
                resolved.profile.tool_names().len(),
                resolved.source
            );
            if stdio {
                // stdout carries protocol messages only.
                eprintln!("[knobyte mcp] {}", profile_line);
                start_stdio_server_with(resolved.profile)?;
            } else {
                let transport = match (http, sse) {
                    (true, _) => HttpTransport::StreamableHttp,
                    (_, true) => HttpTransport::Sse,
                    _ => HttpTransport::Both,
                };
                println!("{} Using {}", "[mcp]".cyan().bold(), profile_line);
                let options = SseServerOptions {
                    token: std::env::var(knobyte::mcp::TOKEN_ENV_VAR).ok(),
                    transport,
                    profile: Some(resolved.profile),
                };
                start_sse_server_with_options(&host, port, options).await?;
            }
        }
        Some(Commands::Pattern { sub }) => match sub {
            PatternCommands::Add { name } => match knobyte::pattern::add_pattern(&config, &name) {
                Ok(added) => {
                    println!("{} Created pattern {}", "[ok]".green().bold(), added.path.display());
                    if added.indexed {
                        println!("{}", "  Added an entry to patterns/INDEX.md".dimmed());
                        println!(
                            "{} Edit patterns/INDEX.md and replace [description] with a real use case.",
                            "[info]".cyan().bold()
                        );
                    } else {
                        println!("{} patterns/INDEX.md not found; list the pattern there yourself.", "[warn]".yellow().bold());
                    }
                }
                Err(e) => {
                    eprintln!("{} {}", "[error]".red().bold(), e);
                    std::process::exit(1);
                }
            },
        },
        Some(Commands::Logging {
            mode,
            expected_revision,
            json,
        }) => {
            use knobyte::agent_logging::{read_policy, render_policy, set_policy, LoggingError};
            let result: Result<(knobyte::agent_logging::LoggingPolicy, bool), LoggingError> = match &mode {
                None if expected_revision.is_some() => Err(LoggingError::Usage(
                    "--expected-revision requires a mode: knobyte logging <significant|checkpoints|manual> --expected-revision <revision|none>".into(),
                )),
                None => read_policy(&config).map(|p| (p, false)),
                Some(m) => set_policy(&config, m, expected_revision.as_deref()).map(|p| (p, true)),
            };
            match result {
                Ok((policy, changed)) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({ "schemaVersion": 1, "command": "logging", "ok": true, "scope": "checkout", "data": policy, "problem": null })
                        );
                    } else {
                        println!("{}", render_policy(&policy, changed));
                    }
                }
                Err(e) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({ "schemaVersion": 1, "command": "logging", "ok": false, "scope": "checkout", "data": null, "problem": { "code": e.code(), "detail": e.detail() } })
                        );
                    } else {
                        eprintln!("{}: {}", e.code(), e.detail());
                    }
                    std::process::exit(e.exit_code());
                }
            }
        }
        Some(Commands::Watch { uninstall, interval }) => {
            if let Some(minutes) = interval {
                let minutes = minutes.unwrap_or_else(|| knobyte::config::watch_interval_minutes(&config.scaffold_root)).max(1);
                println!(
                    "{}",
                    format!("knobyte heartbeat running every {} minute{}. Press Ctrl+C to stop.", minutes, if minutes == 1 { "" } else { "s" }).green()
                );
                loop {
                    let r = run_heartbeat(&config, knobyte::heartbeat::configured_stale_days(&config), false);
                    if r.heartbeat_ok {
                        println!("{} HEARTBEAT_OK", chrono::Local::now().format("%H:%M"));
                    } else {
                        println!(
                            "{} Heartbeat needs attention: {} stale file(s){}{}",
                            chrono::Local::now().format("%H:%M"),
                            r.stale_files.len(),
                            if r.memory_cleanup_due { ", memory cleanup due" } else { "" },
                            r.uncommitted_steps_warning.as_deref().map(|w| format!(", {}", w)).unwrap_or_default()
                        );
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)) => {}
                        _ = tokio::signal::ctrl_c() => {
                            println!("{}", "knobyte heartbeat stopped.".dimmed());
                            break;
                        }
                    }
                }
            } else {
                let res = if uninstall {
                    knobyte::watch::uninstall_hook(&config.project_root)
                } else {
                    let exe = std::env::current_exe()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| "knobyte".to_string());
                    knobyte::watch::install_hook(&config.project_root, &exe)
                };
                match res {
                    Ok(outcome) => println!("{} {}", "[ok]".green().bold(), outcome.message()),
                    Err(e) => {
                        eprintln!("{} {}", "[error]".red().bold(), e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Some(Commands::Completion { shell }) => {
            match knobyte::completion::generate(&shell, "knobyte", &command_tree()) {
                Ok(script) => print!("{}", script),
                Err(e) => {
                    eprintln!("{} {}", "[error]".red().bold(), e);
                    std::process::exit(2);
                }
            }
        }
        Some(Commands::Tui) => {
            if let Err(e) = knobyte::tui::run_tui(&config) {
                println!("{}", e);
            }
        }
        Some(Commands::Cozo { sub }) => run_cozo_command(&config, sub)?,
        Some(Commands::CommandList) => print_commands(),
    }

    Ok(())
}

/// Sync graph.db and wiki.db into Cozo (re-embedding when the embedder changed).
/// Returns (nodes, edges, wiki entities) synchronized.
fn sync_cozo(
    engine: &CozoEngine,
    config: &knobyte::config::KnobyteConfig,
) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    let (mut nodes, mut edges, mut wiki) = (0, 0, 0);
    if let Ok(graph_conn) = rusqlite::Connection::open(config.graph_db_path()) {
        (nodes, edges) = engine.sync_from_graph(&graph_conn)?;
    }
    if let Ok(wiki_conn) = rusqlite::Connection::open(config.wiki_db_path()) {
        wiki = engine.sync_from_wiki(&wiki_conn)?;
    }
    Ok((nodes, edges, wiki))
}

fn run_cozo_model_command(
    config: &knobyte::config::KnobyteConfig,
    sub: CozoModelCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use knobyte::config::{EmbeddingBackend, EmbeddingConfig};
    use knobyte::cozo::model2vec::validate_repo;
    use knobyte::cozo::{embedder_from_config, embedding_status, pull_model};

    match sub {
        CozoModelCommands::Pull { model, force, json } => {
            if !json {
                println!(
                    "Downloading Model2Vec model '{}' from huggingface.co...",
                    model.bold()
                );
            }
            let report = pull_model(&model, force, !json)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                if report.downloaded.is_empty() {
                    println!("All files already present (use --force to re-download).");
                }
                println!(
                    "{} Model '{}' ready: {}-dim, {} at {}",
                    "[ok]".green().bold(),
                    report.model,
                    report.dim,
                    format_bytes(report.total_bytes),
                    report.path
                );
                if config.embedding.backend != EmbeddingBackend::Model2vec
                    || config.embedding.model_repo() != report.model
                {
                    println!(
                        "Activate it with: knobyte cozo model use model2vec --model {}",
                        report.model
                    );
                }
            }
        }
        CozoModelCommands::Status { json } => {
            let status = embedding_status(&config.embedding);
            let cozo_path = config.cozo_db_path();
            let mut indices = serde_json::Map::new();
            if cozo_path.exists() {
                if let Ok(engine) = CozoEngine::open(&cozo_path) {
                    for rel in ["code_nodes", "wiki_entities"] {
                        let v = match engine.stored_space(rel) {
                            Ok(Some((id, dim))) => serde_json::json!({
                                "embedder_id": id,
                                "dim": dim,
                                "matches_config": id == status.embedder_id
                                    && Some(dim) == status.dim,
                            }),
                            _ => serde_json::Value::Null,
                        };
                        indices.insert(rel.to_string(), v);
                    }
                }
            }
            if json {
                let mut v = serde_json::to_value(&status)?;
                v["indices"] = serde_json::Value::Object(indices);
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("Embedding backend: {}", status.backend.bold());
                if let Some(m) = &status.model {
                    println!("Model:             {}", m);
                }
                if let Some(p) = &status.model_path {
                    println!(
                        "Model path:        {} ({})",
                        p,
                        if status.model_present {
                            "present".green()
                        } else {
                            "missing - run 'knobyte cozo model pull'".yellow()
                        }
                    );
                }
                println!(
                    "Dimension:         {}",
                    status
                        .dim
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                );
                println!("Embedder id:       {}", status.embedder_id);
                for (rel, v) in &indices {
                    if v.is_null() {
                        continue;
                    }
                    let ok = v["matches_config"].as_bool().unwrap_or(false);
                    println!(
                        "Index {:<15} {} ({}-dim) {}",
                        format!("{}:", rel),
                        v["embedder_id"].as_str().unwrap_or("?"),
                        v["dim"],
                        if ok {
                            "[current]".green()
                        } else {
                            "[stale - run 'knobyte cozo sync']".yellow()
                        }
                    );
                }
            }
        }
        CozoModelCommands::Use {
            backend,
            model,
            json,
        } => {
            let backend = EmbeddingBackend::parse(&backend).ok_or_else(|| {
                format!(
                    "Unknown embedding backend '{}': expected 'hashed' or 'model2vec'",
                    backend
                )
            })?;
            let new_cfg = match backend {
                EmbeddingBackend::Hashed => {
                    if model.is_some() {
                        return Err("--model only applies to the model2vec backend".into());
                    }
                    EmbeddingConfig {
                        backend,
                        model: None,
                    }
                }
                EmbeddingBackend::Model2vec => {
                    let repo = model.unwrap_or_else(|| config.embedding.model_repo());
                    validate_repo(&repo)?;
                    EmbeddingConfig {
                        backend,
                        model: Some(repo),
                    }
                }
            };
            // Refuses (with a hint to run `pull`) when the model is not downloaded.
            let embedder = embedder_from_config(&new_cfg)?;
            let mut cfg = config.clone();
            cfg.save_embedding(new_cfg)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "embedding": cfg.embedding,
                        "embedder_id": embedder.id(),
                        "dim": embedder.dim(),
                    }))?
                );
            } else {
                println!(
                    "{} Embedding backend set to {} ({}-dim). Vector indices are re-embedded on the next 'knobyte cozo sync' (or search).",
                    "[ok]".green().bold(),
                    embedder.id().bold(),
                    embedder.dim()
                );
            }
        }
    }
    Ok(())
}

fn run_cozo_command(
    config: &knobyte::config::KnobyteConfig,
    sub: CozoCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    let sub = match sub {
        CozoCommands::Model { sub } => return run_cozo_model_command(config, sub),
        other => other,
    };
    let cozo_path = config.cozo_db_path();
    let engine = CozoEngine::open_configured(config)?;

    match sub {
        CozoCommands::Model { .. } => unreachable!("handled above"),
        CozoCommands::Query {
            script,
            params,
            mutable,
            json: _,
        } => {
            let params_json = if let Some(p) = params {
                serde_json::from_str(&p)?
            } else {
                serde_json::json!({})
            };
            let res = if mutable {
                engine.datalog_query_mutable(&script, params_json)?
            } else {
                engine.datalog_query(&script, params_json)?
            };
            println!("{}", serde_json::to_string_pretty(&res)?);
        }
        CozoCommands::Search {
            query,
            target,
            k,
            min_score,
            json,
        } => {
            if let Some(reason) = engine.space_mismatch(&target)? {
                eprintln!(
                    "{} {}\nRe-embedding with '{}' now...",
                    "[info]".cyan(),
                    reason,
                    engine.embedder().id()
                );
                sync_cozo(&engine, config)?;
            }
            let outcome = engine.vector_search_with(
                &query,
                &target,
                &knobyte::cozo::VectorSearchOptions { k, min_score },
            )?;
            let matches = &outcome.matches;
            let floor_note = (outcome.below_floor > 0).then(|| {
                format!(
                    "{} of the {} nearest candidate(s) scored below the relevance floor {:.2} and were left out; lower it with --min-score (0 disables it).",
                    outcome.below_floor, outcome.k, outcome.min_score
                )
            });
            if json {
                println!("{}", serde_json::to_string_pretty(matches)?);
                if let Some(note) = &floor_note {
                    eprintln!("{} {}", "[info]".cyan(), note);
                }
            } else {
                println!(
                    "Vector search results for '{}' (target: {}, embedder: {}):",
                    query.bold(),
                    target.cyan(),
                    engine.embedder().id()
                );
                if matches.is_empty() {
                    println!(
                        "No matches{}.",
                        if outcome.below_floor > 0 { " above the relevance floor" } else { " (the index is empty; run `knobyte cozo sync`)" }
                    );
                }
                for (i, m) in matches.iter().enumerate() {
                    let dist_str = format!("score: {:.3}, dist: {:.3}", m.score, m.distance);
                    let label = m
                        .metadata
                        .get("ref")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&m.id);
                    println!("{}. {} [{}]", i + 1, label.bold(), dist_str.dimmed());
                    for (k, v) in &m.metadata {
                        if k == "ref" || k == "qualified_name" || k == "file_path" {
                            continue;
                        }
                        println!("   {}: {}", k.cyan(), v);
                    }
                }
                if let Some(note) = &floor_note {
                    println!("{} {}", "[info]".cyan(), note);
                } else if !matches.is_empty() && matches.len() < outcome.k {
                    println!(
                        "{} Only {} {} indexed for this target.",
                        "[info]".cyan(),
                        matches.len(),
                        if target == "wiki" { "wiki entities are" } else { "code nodes are" }
                    );
                }
            }
        }
        CozoCommands::Pagerank {
            damping,
            iterations,
            json,
        } => {
            let ranks = engine.pagerank(Some(damping), Some(iterations))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&ranks)?);
            } else {
                println!(
                    "PageRank Centrality Scores (damping: {}, iterations: {}):",
                    damping, iterations
                );
                for (i, r) in ranks.iter().take(20).enumerate() {
                    let label = r.readable_ref.as_deref().unwrap_or(&r.id);
                    println!("{}. {} - rank: {:.6}", i + 1, label.bold(), r.rank);
                }
            }
        }
        CozoCommands::ShortestPath {
            start,
            target,
            json,
        } => {
            let path = engine.shortest_path_detailed(&start, &target)?;
            if json {
                let ids: Option<Vec<String>> =
                    path.map(|steps| steps.into_iter().map(|s| s.id).collect());
                println!("{}", serde_json::to_string_pretty(&ids)?);
            } else if let Some(p) = path {
                println!(
                    "Shortest path from '{}' to '{}' (length {}):",
                    start.bold(),
                    target.bold(),
                    p.len()
                );
                for (i, step) in p.iter().enumerate() {
                    let label = step.readable_ref.as_deref().unwrap_or(&step.id);
                    if i == 0 {
                        println!("  {}", label.cyan());
                    } else {
                        println!("  -> {}", label.cyan());
                    }
                }
            } else {
                println!("No path found between '{}' and '{}'", start, target);
            }
        }
        CozoCommands::Sync { json } => {
            let (nodes_synced, edges_synced, wiki_synced) = sync_cozo(&engine, config)?;
            let embedder = engine.embedder();

            if json {
                let res = serde_json::json!({
                    "status": "ok",
                    "nodes_synced": nodes_synced,
                    "edges_synced": edges_synced,
                    "wiki_synced": wiki_synced,
                    "embedder": embedder.id(),
                    "dim": embedder.dim(),
                    "cozo_db": cozo_path.display().to_string(),
                });
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!(
                    "{} Synchronized SQLite storage to CozoDB:",
                    "[ok]".green().bold()
                );
                println!(
                    "  - Code Nodes:  {} with {}-dim embeddings",
                    nodes_synced,
                    embedder.dim()
                );
                println!("  - Code Edges:  {}", edges_synced);
                println!(
                    "  - Wiki Pages:  {} with {}-dim embeddings",
                    wiki_synced,
                    embedder.dim()
                );
                println!("  - Embedder:    {}", embedder.id());
                println!("  - Storage:     {}", cozo_path.display());
            }
        }
    }
    Ok(())
}

/// Team commands report failures through their envelope; propagate the exit code.
fn exit_on_failure(code: i32) {
    if code != 0 {
        std::process::exit(code);
    }
}

/// Top-level commands and their subcommands, from the CLI definition (for completion).
fn command_tree() -> knobyte::completion::CommandTree {
    use clap::CommandFactory;
    Cli::command()
        .get_subcommands()
        .filter(|c| c.get_name() != "help")
        .map(|c| {
            (
                c.get_name().to_string(),
                c.get_subcommands()
                    .map(|s| s.get_name().to_string())
                    .filter(|n| n != "help")
                    .collect(),
            )
        })
        .collect()
}

fn print_commands() {
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        ("Setup & maintenance", &[
            ("knobyte [--port N] [--no-open]", "Launch the Project Hub (127.0.0.1:4000; guided setup if needed)"),
            ("knobyte setup [--cli] [--tools ...] [--dry-run]", "Scaffold, AI tool anchors, skills, code graph, population, finalize"),
            ("knobyte setup --launch-agent | --no-agent", "Launch Claude Code/Codex to populate (explicit) or print the prompt"),
            ("knobyte init [--json]", "Pre-analysed brief: manifests, entry points, folders, tooling, README"),
            ("knobyte update [--dry-run]", "Refresh Knobyte-owned files and managed blocks; populated content untouched"),
            ("knobyte skills sync [--tool claude|codex] [--backup]", "Install official agent skills without clobbering edits"),
            ("knobyte pattern add <name>", "Create a pattern file and list it in patterns/INDEX.md"),
            ("knobyte logging [significant|checkpoints|manual]", "Read or set this checkout's agent logging mode"),
            ("knobyte watch [--uninstall] [--interval [min]]", "Post-commit drift check hook, or periodic heartbeat"),
            ("knobyte completion <bash|zsh|fish>", "Print a shell completion script"),
            ("knobyte tui", "Interactive terminal dashboard"),
        ]),
        ("Drift & health", &[
            ("knobyte check [--fix] [--json] [--quiet]", "Drift score of the scaffold against code, files and git"),
            ("knobyte sync [--dry-run] [--warnings]", "Relocate groundings, then repair drift with an agent or prompts"),
            ("knobyte sync --launch-agent | --print-prompt", "Launch the repair agent (explicit) or print the prompt"),
            ("knobyte heartbeat [--clean] [--stale-days N]", "Stale scaffold files, memory retention, temp-file cleanup"),
            ("knobyte doctor [--json]", "Health summary with next steps (exit 1 on drift errors)"),
            ("knobyte capabilities --json", "Machine-readable capability manifest for agents"),
        ]),
        ("Code graph", &[
            ("knobyte graph [--root DIR]", "Build the code graph"),
            ("knobyte graph status|refresh|rebuild|repair", "Inspect, refresh, rebuild or repair the graph"),
            ("knobyte graph query <rel> <target>", "where-defined, who-calls, what-calls, who-imports"),
            ("knobyte graph scope <task>", "Bounded source-backed context for a task"),
            ("knobyte graph get <ids...> [--source]", "Nodes by id or grounding reference"),
            ("knobyte graph ground", "Re-baseline grounded references to the current code"),
            ("knobyte impact <target> [--depth N]", "Blast radius of a symbol or file"),
        ]),
        ("Wiki", &[
            ("knobyte wiki list|show|query", "Browse and search wiki entities"),
            ("knobyte wiki related|backlinks|for-code|graph|trace", "Navigate relations, code groundings and traceability"),
            ("knobyte wiki validate|rebuild-index|regenerate-views", "Validate the wiki, rebuild the index, refresh views"),
            ("knobyte wiki apply <file> [--apply]", "Plan or apply typed wiki operations"),
            ("knobyte wiki synthesis ...", "Agent-driven synthesis from the code graph"),
            ("knobyte wiki migrate [--apply]", "Rewrite older Knobyte wiki formats through audited operations"),
            ("knobyte wiki index status|dump|doctor", "Index state, normalized dump, integrity and rebuild diff"),
            ("knobyte export [--out PATH]", "Whole scaffold as one Markdown bundle"),
        ]),
        ("CozoDB & vectors", &[
            ("knobyte cozo search <query>", "HNSW vector search on code and wiki"),
            ("knobyte cozo query <script>", "Datalog query in CozoDB"),
            ("knobyte cozo pagerank|shortest-path", "Graph algorithms over code dependencies"),
            ("knobyte cozo sync", "Sync graph.db and wiki.db into CozoDB"),
            ("knobyte cozo model status|pull|use", "Embedding backend and local models"),
        ]),
        ("Team memory", &[
            ("knobyte log <message> [--kind]", "Append a decision, discovery, note, risk or todo"),
            ("knobyte timeline", "Search the event history"),
            ("knobyte member list|show|current|add|update", "Team members and the effective identity"),
            ("knobyte member select|clear|deactivate|reactivate", "Checkout selection and member lifecycle"),
            ("knobyte activity list|show|record|timeline", "Canonical activity history"),
            ("knobyte workstream list|show|create|update|archive", "Team workstreams"),
            ("knobyte spec list|show <id>", "Requirement specs"),
            ("knobyte inbox target|contract", "Resolve a correction target / request schemas"),
            ("knobyte inbox draft save|list|delete", "Prepare a knowledge proposal draft"),
            ("knobyte inbox publish <draft-id>", "Publish a draft as a pending proposal"),
            ("knobyte inbox proposal list|show", "Review published proposals"),
            ("knobyte inbox approve|reject|withdraw <id>", "Human review decisions on a proposal"),
            ("knobyte relay contract|draft|list|show", "Prepare and inspect handoff relays"),
            ("knobyte relay publish|acknowledge|close <id>", "Relay lifecycle"),
            ("knobyte playbook list|show|create|update|archive", "Reusable team playbooks"),
            ("knobyte playbook run start|list|show|complete-step|abandon", "Playbook runs with step evidence"),
            ("knobyte catch-up [mark|reset]", "Digest of changes since you last caught up"),
        ]),
        ("Servers", &[
            ("knobyte hub [--port] [--host] [--token] [--no-open]", "Project Hub web interface (prints a one-time sign-in link)"),
            ("knobyte mcp [--http|--sse] [--port 3005] [--token]", "HTTP MCP server: /mcp and /sse by default (127.0.0.1)"),
            ("knobyte mcp --stdio", "stdio MCP server for Cursor / Claude Desktop"),
            ("knobyte mcp --profile core|team|wiki|graph|full", "Tool profile to list (default core)"),
            ("knobyte commands", "Print this list"),
        ]),
    ];
    println!("{}", "=== Knobyte CLI Commands ===".bold());
    let width = SECTIONS
        .iter()
        .flat_map(|(_, cmds)| cmds.iter().map(|(c, _)| c.len()))
        .max()
        .unwrap_or(0);
    for (title, cmds) in SECTIONS {
        println!("\n{}", title.bold());
        for (cmd, desc) in cmds.iter() {
            println!("  {:<width$}  {}", cmd, desc, width = width);
        }
    }
}
