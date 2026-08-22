//! The full `knobyte setup` flow: detect the project and the developer's AI tools, scaffold,
//! wire each tool (instruction files, skills, MCP server registration), index (scan, code
//! graph, vector index, wiki index), populate (agent launch with confirmation, never a pause),
//! finalize, print the proof summary, and offer a scoped commit as the last question.
//!
//! Interactive prompts, at most three: the detected tool list (accept or edit), launching the
//! population agent (only when an agent CLI is available) and the commit. Windsurf, whose MCP
//! configuration is user-level only, adds a confirmation naming that file unless
//! `--global-mcp` was passed.
//!
//! When no agent populates the scaffold, setup does not stop: the docs keep their populate
//! markers, everything else is finalized, and the agent instructions tell the first agent
//! session to complete population and run `knobyte setup --finish`. The checkout-local
//! [`SETUP_PENDING_MARKER`] records that the next `--finish` (or re-run) still has to capture
//! grounding baselines.
//!
//! Re-running setup on a populated scaffold (a later run, or a fresh clone of a repository
//! that uses Knobyte) leaves tracked content alone: it creates missing directories, rebuilds
//! the local indexes and reports what `knobyte update`, `--tools` or `--capture-baselines`
//! would change.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use colored::Colorize;

use crate::agent::{
    confirm, find_on_path, is_interactive, launch_permitted, preview_command, print_event, print_launch_preview,
    prompt_line, run_agent, select_agent, AgentTool, LaunchOptions,
};
use crate::config::{load_ai_tools, save_ai_tools, KnobyteConfig, AI_TOOLS};
use crate::setup::anchor::{describe_anchor, ensure_tool_anchors, tool_display_name, AnchorOutcome, TOOL_ANCHORS};
use crate::setup::detect::{detect_tools, DetectEnv, DetectedTool, FALLBACK_TOOLS};
use crate::setup::mcp_register::{self, mcp_targets, project_mcp_paths, register_mcp, McpOptions, McpOutcome, McpResult, ServerCommand};
use crate::setup::prompts::build_population_prompt;
use crate::setup::summary::{central_symbol, run_indexing, sync_wiki_vectors, IndexReport, SetupSummary, ToolWiring};
use crate::setup::templates::REQUIRED_POPULATED_FILES;
use crate::setup::{apply_setup, detect_project_state, is_scaffold_populated, resolve_setup_mode, unpopulated_files, ProjectState};
use crate::skills::{sync_agent_assets, SkillSyncOptions};

/// Population sessions are user-driven but still bounded.
pub const SETUP_AGENT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Default)]
pub struct SetupFlowOptions {
    /// Explicit mode; `None` keeps the saved mode (or `code-repo`).
    pub mode: Option<String>,
    pub dry_run: bool,
    /// Explicit AI tool selection (overrides detection and the saved `aiTools`). Empty = none.
    pub tools: Option<Vec<String>>,
    /// Interactive terminal flow (`--cli`, or a TTY).
    pub interactive: bool,
    /// Launch the agent without asking (explicit consent).
    pub launch_agent: bool,
    /// Never launch an agent.
    pub no_agent: bool,
    /// Agent to launch (`claude` / `codex`); defaults to the first selected, installed one.
    pub agent: Option<String>,
    /// Skip building the code graph.
    pub skip_graph: bool,
    /// Create the commit checkpoint without asking.
    pub commit: bool,
    /// Move conflicting skill directories aside instead of reporting a conflict.
    pub backup_skills: bool,
    /// Capture missing grounding baselines into the Markdown even when re-running setup on
    /// an already populated scaffold.
    pub capture_baselines: bool,
    /// Do not register Knobyte's MCP server with the selected tools.
    pub no_mcp: bool,
    /// Consent to write user-level MCP configuration (Windsurf).
    pub global_mcp: bool,
    /// `--finish`: after population, re-scan, finalize, capture baselines and report.
    pub finish: bool,
    /// Where tool detection looks (`None`: this process's PATH and home directory).
    pub detect_env: Option<DetectEnv>,
}

/// Checkout-local marker (under `.knobyte/local/`, never committed) written while population
/// is pending, so the next `--finish` or re-run knows it is completing a fresh setup (and
/// captures grounding baselines) rather than re-running on an established scaffold.
pub const SETUP_PENDING_MARKER: &str = "setup-pending";

fn pending_marker(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join(SETUP_PENDING_MARKER)
}

/// Whether a fresh setup is still waiting for population (or its `--finish`).
pub fn setup_pending(config: &KnobyteConfig) -> bool {
    pending_marker(config).exists()
}

/// Groundings (document, reference) in the scaffold Markdown that carry no committed
/// baseline (`knobyte graph ground --rebaseline` captures them).
pub fn unbaselined_groundings(scaffold_root: &Path) -> usize {
    use crate::graph::grounding::{extract_doc_refs, resolve_baseline, scaffold_markdown_files, CommittedIndex};
    let docs: Vec<(String, Vec<crate::graph::grounding::DocRef>)> = scaffold_markdown_files(scaffold_root)
        .into_iter()
        .filter_map(|(rel, path)| std::fs::read_to_string(path).ok().map(|c| (rel, extract_doc_refs(&c))))
        .collect();
    let index = CommittedIndex::build(docs.iter().map(|(d, r)| (d.as_str(), r.as_slice())));
    let mut seen = std::collections::BTreeSet::new();
    docs.iter()
        .flat_map(|(doc, refs)| refs.iter().map(move |r| (doc, r)))
        .filter(|(doc, r)| seen.insert((doc.to_string(), r.reference.clone())))
        .filter(|(doc, r)| resolve_baseline(None, doc, r, &index).body_hash.is_none())
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStage {
    /// Setup finished; the docs still carry populate markers for the first agent session.
    NeedsPopulation,
    Ready,
    DryRun,
}

pub struct SetupFlowResult {
    pub mode: String,
    pub stage: SetupStage,
    pub tools: Vec<String>,
    pub detected: Vec<DetectedTool>,
    pub prompt: Option<String>,
    pub anchor_notes: Vec<String>,
    pub mcp: Vec<McpResult>,
    pub summary: Option<SetupSummary>,
}

fn ok(msg: impl AsRef<str>) {
    println!("{} {}", "[ok]".green().bold(), msg.as_ref());
}
fn info(msg: impl AsRef<str>) {
    println!("{} {}", "[info]".cyan().bold(), msg.as_ref());
}
fn warn(msg: impl AsRef<str>) {
    println!("{} {}", "[warn]".yellow().bold(), msg.as_ref());
}
fn header(msg: &str) {
    println!("\n{}", msg.bold());
}

/// Parse a comma/space separated tool list (`claude,cursor`, `none`).
pub fn parse_tool_list(raw: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw.split([',', ' ']).map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()) {
        if t == "none" {
            continue;
        }
        let t = match t.as_str() {
            "claude-code" => "claude".to_string(),
            "vscode" | "code" => "copilot".to_string(),
            _ => t,
        };
        if !AI_TOOLS.contains(&t.as_str()) {
            return Err(format!("Unknown AI tool '{}'. Valid tools: {}, none", t, AI_TOOLS.join(", ")));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    Ok(out)
}

fn tool_names(tools: &[String]) -> String {
    if tools.is_empty() {
        "none".into()
    } else {
        tools.iter().map(|t| tool_display_name(t)).collect::<Vec<_>>().join(", ")
    }
}

/// Show the proposed tools once and take one answer: accept, or edit the list.
fn confirm_tools(proposed: Vec<String>) -> Vec<String> {
    let answer = prompt_line(&format!("Set up Knobyte for {}? [Y/n/e = edit list] ", tool_names(&proposed))).unwrap_or_default();
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => proposed,
        "n" | "no" => {
            info("No AI tool files will be written; .knobyte/AGENTS.md works with any tool that can read files");
            Vec::new()
        }
        _ => {
            let raw = prompt_line(&format!(
                "Tools (comma separated: {}, or none) [{}]: ",
                AI_TOOLS.join(", "),
                if proposed.is_empty() { "none".to_string() } else { proposed.join(",") }
            ))
            .unwrap_or_default();
            if raw.trim().is_empty() {
                return proposed;
            }
            match parse_tool_list(&raw) {
                Ok(t) => t,
                Err(e) => {
                    warn(format!("{}; keeping {}", e, tool_names(&proposed)));
                    proposed
                }
            }
        }
    }
}

/// How the tool list was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSource {
    Explicit,
    Saved,
    Detected,
    Fallback,
}

fn choose_tools(config: &KnobyteConfig, opts: &SetupFlowOptions, detected: &[DetectedTool], interactive: bool) -> (Vec<String>, ToolSource) {
    if let Some(t) = &opts.tools {
        return (t.clone(), ToolSource::Explicit);
    }
    if let Some(t) = load_ai_tools(&config.scaffold_root) {
        info(format!("Using configured AI tools: {}", tool_names(&t)));
        return (t, ToolSource::Saved);
    }
    let (proposed, source) = if detected.is_empty() {
        info("No AI tools detected; writing AGENTS.md and CLAUDE.md, which most coding agents read (pass --tools to choose)");
        (FALLBACK_TOOLS.iter().map(|s| s.to_string()).collect::<Vec<_>>(), ToolSource::Fallback)
    } else {
        info("Detected:");
        for d in detected {
            println!("    {:<16} {}", tool_display_name(&d.tool), d.signals.join(", "));
        }
        (detected.iter().map(|d| d.tool.clone()).collect(), ToolSource::Detected)
    };
    if interactive {
        let chosen = confirm_tools(proposed.clone());
        let source = if chosen == proposed { source } else { ToolSource::Explicit };
        (chosen, source)
    } else {
        (proposed, source)
    }
}

/// Exact git paths for the commit checkpoint: the scaffold and the files written for `tools`
/// (instruction files, skills and project-scoped MCP configuration).
pub fn commit_checkpoint_paths(config: &KnobyteConfig, tools: &[String]) -> Vec<String> {
    let scaffold = config
        .scaffold_root
        .strip_prefix(&config.project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| ".knobyte".into());
    let mut paths = vec![scaffold];
    let has = |t: &str| tools.iter().any(|x| x == t);
    if has("claude") {
        paths.extend(["CLAUDE.md", ".claude/skills/knobyte-inbox", ".claude/skills/knobyte-relay"].map(String::from));
    }
    if has("codex") {
        paths.extend(["AGENTS.md", ".agents/skills/knobyte-inbox", ".agents/skills/knobyte-relay"].map(String::from));
    }
    for (tool, path, _) in TOOL_ANCHORS {
        if has(tool) {
            paths.push(path.to_string());
        }
    }
    for p in project_mcp_paths(tools) {
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    if config.project_root.join(".gitignore").exists() {
        paths.push(".gitignore".into());
    }
    paths
}

const CHECKPOINT_MESSAGE: &str = "chore: initialize Knobyte project memory";

fn print_commit_commands(paths: &[String]) {
    println!("    git add -- {}", paths.join(" "));
    println!("    git commit -m \"{}\"", CHECKPOINT_MESSAGE);
}

/// Split checkpoint `paths` into (existing paths git will stage, existing paths git ignores).
/// Ignored paths respect every ignore source (`.gitignore` files, `.git/info/exclude`, the
/// global excludes file); tracked files are never reported as ignored.
pub fn split_ignored_paths(project_root: &Path, paths: &[String]) -> (Vec<String>, Vec<String>) {
    let existing: Vec<String> = paths.iter().filter(|p| project_root.join(p).exists()).cloned().collect();
    if existing.is_empty() {
        return (existing, Vec::new());
    }
    let ignored: Vec<String> = Command::new("git")
        .args(["check-ignore", "--"])
        .args(&existing)
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.code() == Some(0))
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().trim_end_matches('/').to_string()).collect())
        .unwrap_or_default();
    let (ignored, kept): (Vec<String>, Vec<String>) =
        existing.into_iter().partition(|p| ignored.iter().any(|i| i == p.trim_end_matches('/')));
    (kept, ignored)
}

/// Stage exactly `paths` (minus missing and git-ignored ones) and commit them. Never pushes.
pub fn create_commit_checkpoint(project_root: &Path, paths: &[String]) -> Result<String, String> {
    let (existing, _ignored) = split_ignored_paths(project_root, paths);
    if existing.is_empty() {
        return Err("none of Knobyte's files can be staged".into());
    }
    let add = Command::new("git")
        .arg("add")
        .arg("--")
        .args(existing.iter().map(|s| s.as_str()))
        .current_dir(project_root)
        .output()
        .map_err(|e| e.to_string())?;
    if !add.status.success() {
        return Err(format!(
            "`git add` failed, so nothing was staged or committed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }
    let commit = Command::new("git")
        .args(["commit", "-m", CHECKPOINT_MESSAGE, "--"])
        .args(existing.iter().map(|s| s.as_str()))
        .current_dir(project_root)
        .output()
        .map_err(|e| e.to_string())?;
    if !commit.status.success() {
        let msg = format!("{}{}", String::from_utf8_lossy(&commit.stdout), String::from_utf8_lossy(&commit.stderr));
        return Err(format!("`git commit` failed, so nothing was committed: {}", msg.trim()));
    }
    Ok(String::from_utf8_lossy(&commit.stdout).lines().next().unwrap_or("").to_string())
}

/// Whether any checkpoint path has uncommitted changes (or git is unavailable).
fn has_checkpoint_changes(project_root: &Path, paths: &[String]) -> bool {
    let existing: Vec<&String> = paths.iter().filter(|p| project_root.join(p).exists()).collect();
    if existing.is_empty() {
        return false;
    }
    Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all", "--"])
        .args(existing.iter().map(|s| s.as_str()))
        .current_dir(project_root)
        .output()
        .map(|o| !o.status.success() || !o.stdout.is_empty())
        .unwrap_or(true)
}

/// Finalize the wiki (see [`crate::wiki::finalize`]): migrate older Knobyte formats, capture
/// grounding baselines, rebuild the index and validate. Fails with a clear message when the
/// wiki is not ready or a grounding could not be baselined. Returns
/// (baselines captured, indexed entities).
pub fn finalize_setup(config: &KnobyteConfig) -> Result<(usize, usize), String> {
    let f = crate::wiki::finalize::finalize_wiki(config);
    if !f.ready {
        return Err(f.failure_message());
    }
    Ok((f.baselines_captured, f.indexed_entities))
}

/// Finalize a scaffold whose population is still pending: rebuild the index and capture
/// whatever baselines exist, reporting problems instead of failing.
pub fn finalize_pending(config: &KnobyteConfig) -> Result<(usize, usize), String> {
    let f = crate::wiki::finalize::finalize_wiki_with(
        config,
        &crate::wiki::finalize::FinalizeOptions { apply_migration: true, capture_baselines: true, strict: false },
    );
    if !f.ready {
        return Err(f.failure_message());
    }
    Ok((f.baselines_captured, f.indexed_entities))
}

/// The population prompt for `config`'s current state (`knobyte setup --print-prompt`).
pub fn population_prompt(config: &KnobyteConfig) -> String {
    let mode = resolve_setup_mode(config, None).unwrap_or_else(|_| "code-repo".into());
    let state = detect_project_state(&config.project_root, &config.scaffold_root);
    let brief = if mode != "agent-memory" && state != ProjectState::Fresh {
        serde_json::to_string_pretty(&crate::scanner::scan(&config.project_root)).ok()
    } else {
        None
    };
    build_population_prompt(&mode, state, brief.as_deref())
}

/// The files that point each tool at Knobyte, for the summary.
fn tool_wiring(tools: &[String], anchors: &[crate::setup::anchor::AnchorResult], mcp: &[McpResult]) -> Vec<ToolWiring> {
    tools
        .iter()
        .map(|t| {
            let mut files = Vec::new();
            match t.as_str() {
                "claude" => files.push("CLAUDE.md".to_string()),
                "codex" => files.push("AGENTS.md".to_string()),
                _ => {}
            }
            for a in anchors.iter().filter(|a| &a.tool == t && a.outcome != AnchorOutcome::Conflict) {
                files.push(a.path.clone());
            }
            if matches!(t.as_str(), "claude" | "codex") {
                files.push("skills".to_string());
            }
            for m in mcp.iter().filter(|m| &m.tool == t && m.registered() && !(m.dry_run && m.changes())) {
                if !files.contains(&m.path) {
                    files.push(format!("{} (MCP)", m.path));
                }
            }
            ToolWiring { tool: t.clone(), name: tool_display_name(t).to_string(), files }
        })
        .collect()
}

/// The proof summary for the current state of `config`.
pub fn build_summary(
    config: &KnobyteConfig,
    tools: &[String],
    anchors: &[crate::setup::anchor::AnchorResult],
    mcp: &[McpResult],
    index: IndexReport,
    docs_created: usize,
    baselines: usize,
) -> SetupSummary {
    let unpopulated = unpopulated_files(&config.scaffold_root).len();
    let drift = crate::drift::checker::run_drift_check(config);
    SetupSummary {
        tools: tool_wiring(tools, anchors, mcp),
        mcp_pending: mcp
            .iter()
            .filter(|m| matches!(m.outcome, McpOutcome::NeedsConsent | McpOutcome::Unparseable | McpOutcome::Failed))
            .map(|m| format!("{}: {}", tool_display_name(&m.tool), m.path))
            .collect(),
        index,
        docs_created,
        docs_total: REQUIRED_POPULATED_FILES.len(),
        docs_populated: REQUIRED_POPULATED_FILES.len().saturating_sub(unpopulated),
        population_pending: unpopulated > 0,
        baselines_captured: baselines,
        drift_score: Some(drift.score),
        central: central_symbol(&config.graph_db_path()),
        not_committed: Vec::new(),
    }
}

/// The proof summary of an existing setup, computed without writing tracked files (the Hub's
/// finalize step): wiring from the saved tools, counts from the indexes.
pub fn current_summary(config: &KnobyteConfig, baselines: usize) -> SetupSummary {
    let tools = load_ai_tools(&config.scaffold_root).unwrap_or_default();
    let anchors = ensure_tool_anchors(&config.project_root, &tools, true);
    let home = crate::setup::detect::home_dir();
    let targets = mcp_targets(&config.project_root, home.as_deref(), &tools);
    let mcp = register_mcp(
        &config.project_root,
        &targets,
        &ServerCommand::resolve(None),
        &McpOptions { dry_run: true, allow_user_files: false },
    );
    build_summary(config, &tools, &anchors, &mcp, crate::setup::summary::current_index_report(config), 0, baselines)
}

/// Run the setup flow.
pub fn run_setup_flow(config: &KnobyteConfig, opts: &SetupFlowOptions) -> Result<SetupFlowResult, String> {
    let mode = resolve_setup_mode(config, opts.mode.as_deref())?;
    let dry = opts.dry_run;
    let interactive = opts.interactive && is_interactive();
    if dry {
        warn("DRY RUN: no files will be created or modified");
    }
    let has_git = config.project_root.join(".git").exists();
    if mode == "code-repo" && !has_git {
        warn("No git repository found. Drift history and the commit checkpoint need one (run `git init`).");
    }

    // 1. Detect project state.
    let populated_at_start = is_scaffold_populated(&config.scaffold_root);
    let state = detect_project_state(&config.project_root, &config.scaffold_root);
    let was_pending = setup_pending(config);
    // A populated scaffold that is not finishing a fresh setup: leave tracked content alone.
    let rerun = populated_at_start && !was_pending;
    // Agent files (anchors, skills, MCP, aiTools) are rewritten on a re-run only when asked.
    let write_agent_files = !rerun || opts.tools.is_some();
    match (mode.as_str(), state) {
        ("agent-memory", _) => info("Detected: agent-memory workspace (persistent-agent operational memory)"),
        (_, ProjectState::Existing) => info("Detected: existing codebase with source files"),
        (_, ProjectState::Fresh) => info("Detected: fresh project (no source files yet)"),
        (_, ProjectState::Partial) => info("Detected: existing codebase with a populated scaffold"),
    }
    if opts.finish {
        info("Finishing setup: re-scan, finalize, capture grounding baselines, report");
    } else if rerun {
        info("The scaffold is already populated: tracked files stay as they are; setup refreshes local state (graph, vectors, wiki index)");
    }

    // 2. Scaffold.
    header("Scaffold");
    let report = apply_setup(config, &mode, dry || rerun)?;
    let mut held_back: Vec<String> = Vec::new();
    let display = |p: &str| {
        Path::new(p)
            .strip_prefix(&config.project_root)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| p.to_string())
    };
    if rerun && !dry {
        for a in report.actions.iter().filter(|a| a.action == "create_dir") {
            std::fs::create_dir_all(&a.path).map_err(|e| format!("{}: {}", a.path, e))?;
        }
    }
    let mut docs_created = 0;
    let scaffold_prefix = display(&config.scaffold_root.to_string_lossy());
    for a in report.actions.iter().filter(|a| a.action != "create_dir") {
        if rerun {
            held_back.push(format!("{} ({})", display(&a.path), a.detail));
            continue;
        }
        if a.action == "create_file" && a.path.ends_with(".md") {
            docs_created += 1;
        }
        let verb = match (dry, a.action.as_str()) {
            (true, "modify_file") => "Would modify",
            (true, _) => "Would create",
            (false, "modify_file") => "Modified",
            (false, _) => "Created",
        };
        let path = display(&a.path);
        // Scaffold documents are summarized; root files are listed.
        if a.action == "create_file" && path.starts_with(&scaffold_prefix) && path.ends_with(".md") {
            continue;
        }
        ok(format!("{} {} ({})", verb, path, a.detail));
    }
    if docs_created > 0 {
        ok(format!("{} {} scaffold documents in {}/", if dry { "Would create" } else { "Created" }, docs_created, scaffold_prefix));
    }
    if !held_back.is_empty() {
        info("The scaffold is already populated, so setup leaves its files unchanged. `knobyte update` would refresh:");
        for h in &held_back {
            println!("    {}", h);
        }
    } else if report.actions.iter().all(|a| a.action == "create_dir") {
        info("Scaffold is up to date; existing files were preserved");
    }

    // 3. AI tools.
    header("AI tools");
    let detect_env = opts.detect_env.clone().unwrap_or_else(|| DetectEnv::from_process(&config.project_root));
    let detected = detect_tools(&detect_env);
    let saved = load_ai_tools(&config.scaffold_root);
    let (tools, source) = choose_tools(config, opts, &detected, interactive && write_agent_files);
    // Agent files are left as they are on a re-run without --tools: report what would change.
    let agent_dry = dry || !write_agent_files;
    if !agent_dry && saved.as_ref() != Some(&tools) {
        save_ai_tools(&config.scaffold_root, &tools).map_err(|e| format!("could not save aiTools: {}", e))?;
    }
    let anchors = ensure_tool_anchors(&config.project_root, &tools, agent_dry);
    let mut anchor_notes = Vec::new();
    let mut agent_files_pending = false;
    for a in &anchors {
        let line = describe_anchor(a, agent_dry);
        match a.outcome {
            AnchorOutcome::Conflict => {
                warn(&line);
                anchor_notes.push(line);
            }
            AnchorOutcome::AlreadyLinked => info(line),
            _ if !write_agent_files => {
                agent_files_pending = true;
                info(line)
            }
            _ => ok(line),
        }
    }

    // Agent skills + managed instruction blocks (Claude Code / Codex).
    let clients: Vec<&str> = tools.iter().map(String::as_str).filter(|t| *t == "claude" || *t == "codex").collect();
    if !clients.is_empty() {
        let assets = sync_agent_assets(
            config,
            &clients,
            SkillSyncOptions { dry_run: agent_dry, check_ignored: mode == "code-repo", backup_conflicts: opts.backup_skills },
        )?;
        for a in &assets.actions {
            match a.action.as_str() {
                "conflict" => {}
                "unchanged" => {}
                _ if !write_agent_files => {
                    agent_files_pending = true;
                    info(format!("(not applied) {}", a.message))
                }
                _ => ok(if dry { format!("(dry run) {}", a.message) } else { a.message.clone() }),
            }
        }
        if assets.actions.iter().all(|a| a.action == "unchanged") && !assets.actions.is_empty() {
            info("Agent skills and instruction blocks are up to date");
        }
        for w in &assets.warnings {
            warn(&w.message);
            if let Some(r) = &w.resolution {
                println!("{}", r.dimmed());
            }
        }
        if assets.conflicted && write_agent_files {
            return Err("Knobyte agent assets have conflicts. Resolve the warnings above (or rerun with --backup-skills) and rerun setup or `knobyte skills sync`.".into());
        }
    }

    // MCP server registration.
    let mut mcp: Vec<McpResult> = Vec::new();
    if opts.no_mcp {
        info("Skipping MCP server registration (--no-mcp)");
    } else if source == ToolSource::Fallback {
        info(format!(
            "No AI tool detected, so no MCP configuration was written; register it later with `knobyte setup --tools <tool>` (server: {})",
            ServerCommand::resolve(None).display()
        ));
    } else if !tools.is_empty() {
        let cmd = ServerCommand::resolve(None);
        let targets = mcp_targets(&config.project_root, detect_env.home.as_deref(), &tools);
        if detect_env.home.is_none() && tools.iter().any(|t| t == "windsurf") {
            info(format!(
                "No home directory is set, so Windsurf's user-level MCP file was not located; add the server there yourself: {}",
                cmd.display()
            ));
        }
        let mut allow_user = opts.global_mcp;
        if !allow_user && interactive && !agent_dry {
            // A user-level file is written only after a confirmation naming it.
            let plan = register_mcp(&config.project_root, &targets, &cmd, &McpOptions { dry_run: true, allow_user_files: false });
            for r in plan.iter().filter(|r| r.outcome == McpOutcome::NeedsConsent) {
                allow_user = confirm(
                    &format!(
                        "{} reads MCP servers only from your user-level {}. Add Knobyte there?",
                        tool_display_name(&r.tool),
                        r.path
                    ),
                    false,
                );
            }
        }
        mcp = register_mcp(&config.project_root, &targets, &cmd, &McpOptions { dry_run: agent_dry, allow_user_files: allow_user });
        for r in &mcp {
            let line = mcp_register::describe(r);
            match r.outcome {
                McpOutcome::Unparseable | McpOutcome::Failed => warn(line),
                McpOutcome::NeedsConsent => info(line),
                McpOutcome::Unchanged => info(line),
                _ if r.dry_run => {
                    agent_files_pending |= !write_agent_files;
                    info(line)
                }
                _ => ok(line),
            }
            if let Some(s) = &r.snippet {
                for l in s.lines() {
                    println!("    {}", l);
                }
            }
        }
    }
    if agent_files_pending {
        info(format!(
            "Agent files were left unchanged. Run `knobyte setup --tools {}` to write them.",
            if tools.is_empty() { "none".to_string() } else { tools.join(",") }
        ));
    }

    let prompt = build_population_prompt(&mode, state, None);
    if dry {
        header("Would index, populate and finalize (dry run; skipping)");
        ok("Done (dry run).");
        return Ok(SetupFlowResult {
            mode,
            stage: SetupStage::DryRun,
            tools,
            detected,
            prompt: Some(prompt),
            anchor_notes,
            mcp,
            summary: None,
        });
    }

    // 4. Indexing: scan → code graph → vector index → wiki index.
    let index: IndexReport = run_indexing(config, mode != "agent-memory", opts.skip_graph)?;

    // 5. Population: launch an agent with consent, otherwise continue without pausing.
    let mut populated = populated_at_start;
    if !populated && !opts.finish {
        header("Population");
        let candidate = match &opts.agent {
            Some(a) => {
                let tool = AgentTool::parse(a).ok_or_else(|| format!("Unknown agent '{}'. Use claude or codex.", a))?;
                find_on_path(tool.program(), None).map(|_| tool)
            }
            None => select_agent(&tools, None),
        };
        match (candidate, opts.no_agent) {
            (Some(tool), false) => match launch_permitted(interactive, opts.launch_agent) {
                Ok(()) => {
                    let launch_opts = LaunchOptions {
                        cwd: config.project_root.clone(),
                        private_dir: config.local_dir(),
                        timeout: Some(SETUP_AGENT_TIMEOUT),
                        path_env: None,
                        allow_non_git: !has_git,
                    };
                    print_launch_preview(tool, &preview_command(tool, &launch_opts), &config.project_root);
                    let go = opts.launch_agent || confirm(&format!("Launch {} to populate the docs now?", tool.display_name()), true);
                    if go {
                        let full_prompt = population_prompt(config);
                        let outcome = run_agent(tool, &full_prompt, &launch_opts, &mut |ev| print_event(ev));
                        match outcome.failure {
                            None => {
                                ok(format!("{} finished the population session", tool.display_name()));
                                populated = is_scaffold_populated(&config.scaffold_root);
                                if !populated {
                                    warn(format!(
                                        "These files still need population: {}",
                                        unpopulated_files(&config.scaffold_root).join(", ")
                                    ));
                                }
                            }
                            Some(f) => warn(f.message(tool)),
                        }
                    } else {
                        info("Not launching an agent.");
                    }
                }
                Err(reason) => info(format!("Not launching {}: {}.", tool.display_name(), reason)),
            },
            (None, false) if !clients.is_empty() || opts.agent.is_some() => {
                info("No Claude Code or Codex CLI on PATH to populate the docs now.")
            }
            _ => {}
        }
        if !populated {
            info("Population pending: the docs keep their \"to fill\" markers. Your first agent session sees them (its Knobyte instructions say so), fills the docs from the code and runs `knobyte setup --finish`.");
            info("To populate by hand instead: `knobyte setup --print-prompt` prints the full prompt.");
        }
    } else if !populated {
        info(format!(
            "Population is still pending: {} carry the populate marker. Fill them, then run `knobyte setup --finish` again.",
            unpopulated_files(&config.scaffold_root).join(", ")
        ));
    }

    // 6. Finalize.
    header("Finalizing");
    let baselines;
    if populated && !rerun {
        let (captured, entities) = finalize_setup(config)?;
        let _ = std::fs::remove_file(pending_marker(config));
        baselines = captured;
        if captured > 0 {
            ok(format!("Captured {} grounding baseline(s)", captured));
        } else {
            info("No authored grounding baselines needed capture");
        }
        ok(format!("Wiki ready with {} indexed entit{}", entities, if entities == 1 { "y" } else { "ies" }));
    } else if !populated {
        let (captured, entities) = finalize_pending(config)?;
        let _ = std::fs::create_dir_all(config.local_dir())
            .and_then(|_| std::fs::write(pending_marker(config), "population pending\n"));
        baselines = captured;
        ok(format!(
            "Wiki index ready with {} entit{}; baselines are captured by `knobyte setup --finish` once the docs are populated",
            entities,
            if entities == 1 { "y" } else { "ies" }
        ));
    } else {
        let capture = opts.capture_baselines || opts.finish;
        let f = crate::wiki::finalize::finalize_wiki_with(
            config,
            &crate::wiki::finalize::FinalizeOptions { capture_baselines: capture, ..crate::wiki::finalize::FinalizeOptions::read_only() },
        );
        if !f.ready {
            return Err(f.failure_message());
        }
        baselines = f.baselines_captured;
        if f.planned_changes > 0 {
            info(format!(
                "{} wiki format migration change(s) pending; review them with `knobyte wiki migrate --dry-run`",
                f.planned_changes
            ));
        }
        let problems = f.diagnostics.len() + f.skipped_groundings.len();
        if problems > 0 {
            warn(format!("{} wiki problem(s) found; run `knobyte wiki validate` and `knobyte check`", problems));
        }
        if capture {
            ok(format!("Captured {} grounding baseline(s); review and commit the Markdown changes", f.baselines_captured));
        } else {
            let missing = unbaselined_groundings(&config.scaffold_root);
            if missing > 0 {
                info(format!(
                    "{} grounding{} no committed baseline — run `knobyte graph ground --rebaseline` to capture them",
                    missing,
                    if missing == 1 { " has" } else { "s have" }
                ));
            }
        }
        ok(format!(
            "Wiki index rebuilt with {} entit{}{}",
            f.indexed_entities,
            if f.indexed_entities == 1 { "y" } else { "ies" },
            if capture { "" } else { "; tracked scaffold files were not modified" }
        ));
    }
    let mut index = index;
    if let Some(v) = index.vector.as_mut() {
        if let Ok(n) = sync_wiki_vectors(config) {
            v.wiki_pages = n;
        }
    }
    if let Ok(entities) = crate::wiki::WikiIndex::open_read_only(&config.wiki_db_path()).and_then(|i| i.entity_count()) {
        index.wiki_entities = entities;
    }

    // 7. Proof.
    let mut summary = build_summary(config, &tools, &anchors, &mcp, index, docs_created, baselines);
    let offer_commit = mode == "code-repo" && has_git && (!rerun || opts.finish || opts.capture_baselines || opts.tools.is_some());
    let (commit_paths, ignored_paths) = if offer_commit {
        split_ignored_paths(&config.project_root, &commit_checkpoint_paths(config, &tools))
    } else {
        (Vec::new(), Vec::new())
    };
    summary.not_committed = ignored_paths;
    print_anchor_notes(&anchor_notes);
    summary.print();

    // 8. Commit checkpoint: the single final question (scoped to Knobyte's files).
    if offer_commit {
        let paths = commit_paths;
        if has_checkpoint_changes(&config.project_root, &paths) {
            header("Commit");
            let go = opts.commit
                || (interactive && confirm("Commit Knobyte's files now (staging only Knobyte's paths, nothing is pushed)?", false));
            if go {
                match create_commit_checkpoint(&config.project_root, &paths) {
                    Ok(line) => ok(format!("Committed: {}", line)),
                    Err(e) => warn(format!("Commit checkpoint failed: {}", e)),
                }
            } else {
                info("Nothing was staged or committed. To commit Knobyte's files later:");
                print_commit_commands(&paths);
            }
        }
    }

    let stage = if populated { SetupStage::Ready } else { SetupStage::NeedsPopulation };
    Ok(SetupFlowResult { mode, stage, tools, detected, prompt: (!populated).then_some(prompt), anchor_notes, mcp, summary: Some(summary) })
}

fn print_anchor_notes(notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    header("Action needed: these files do not point at the scaffold yet");
    for n in notes {
        warn(n);
    }
    info("Until one always-loaded file names `.knobyte/`, your agent will not read the scaffold. Run `knobyte check` after fixing them.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_parsing() {
        assert_eq!(parse_tool_list("claude, cursor claude").unwrap(), vec!["claude", "cursor"]);
        assert_eq!(parse_tool_list("vscode").unwrap(), vec!["copilot"]);
        assert!(parse_tool_list("none").unwrap().is_empty());
        assert!(parse_tool_list("vim").is_err());
    }

    #[test]
    fn checkpoint_paths_include_project_mcp_files() {
        let d = tempfile::tempdir().unwrap();
        let config = KnobyteConfig::new(d.path().to_path_buf(), d.path().join(".knobyte"));
        let tools: Vec<String> = AI_TOOLS.iter().map(|s| s.to_string()).collect();
        let paths = commit_checkpoint_paths(&config, &tools);
        for p in [".mcp.json", ".cursor/mcp.json", ".vscode/mcp.json", "opencode.json", ".codex/config.toml", ".cursorrules"] {
            assert!(paths.contains(&p.to_string()), "{}: {:?}", p, paths);
        }
        assert!(!paths.iter().any(|p| p.contains("windsurf/mcp_config")), "user-level files are never committed");
        assert_eq!(paths.iter().filter(|p| *p == "opencode.json").count(), 1);
    }
}
