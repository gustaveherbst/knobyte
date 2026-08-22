//! The full `knobyte setup` flow: detect state, scaffold, AI tool selection and anchors,
//! agent skills, scan, code graph, population (agent launch with confirmation, or the
//! prompt to paste), then finalize (grounding baselines, wiki index) and an optional,
//! user-confirmed commit checkpoint.
//!
//! Re-running setup on a scaffold that was already populated (a later run, or a fresh clone
//! of a repository that uses Knobyte) leaves tracked content alone: it creates missing
//! directories, rebuilds the local graph and wiki index, reports what `knobyte update` or
//! `--tools` would change, and reports groundings without a committed baseline instead of
//! capturing them (unless `--capture-baselines`). Setup that paused at population (see
//! [`SETUP_PENDING_MARKER`]) still finishes with the full finalization when re-run.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use colored::Colorize;

use crate::agent::{
    confirm, find_on_path, is_interactive, launch_permitted, preview_command, print_event,
    print_launch_preview, print_prompt_for_paste, prompt_line, run_agent, select_agent, AgentTool,
    LaunchOptions,
};
use crate::config::{load_ai_tools, save_ai_tools, KnobyteConfig, AI_TOOLS};
use crate::setup::anchor::{describe_anchor, ensure_tool_anchors, tool_display_name, AnchorOutcome};
use crate::setup::prompts::build_population_prompt;
use crate::setup::{
    apply_setup, detect_project_state, is_scaffold_populated, resolve_setup_mode, unpopulated_files,
    ProjectState,
};
use crate::skills::{sync_agent_assets, SkillSyncOptions};

/// Population sessions are user-driven but still bounded.
pub const SETUP_AGENT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Default)]
pub struct SetupFlowOptions {
    /// Explicit mode; `None` keeps the saved mode (or `code-repo`).
    pub mode: Option<String>,
    pub dry_run: bool,
    /// Explicit AI tool selection (overrides the saved `aiTools`). Empty = none.
    pub tools: Option<Vec<String>>,
    /// Interactive terminal flow (`--cli`, or a TTY).
    pub interactive: bool,
    /// Launch the agent without asking (explicit consent).
    pub launch_agent: bool,
    /// Never launch an agent; print the population prompt instead.
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
}

/// Checkout-local marker (under `.knobyte/local/`, never committed) written when setup pauses
/// at population, so the next run knows it is finishing a fresh setup rather than re-running
/// on an established scaffold.
pub const SETUP_PENDING_MARKER: &str = "setup-pending";

fn pending_marker(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join(SETUP_PENDING_MARKER)
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
    NeedsPopulation,
    Ready,
    DryRun,
}

pub struct SetupFlowResult {
    pub mode: String,
    pub stage: SetupStage,
    pub tools: Vec<String>,
    pub prompt: Option<String>,
    pub anchor_notes: Vec<String>,
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
        let t = if t == "claude-code" { "claude".to_string() } else { t };
        if !AI_TOOLS.contains(&t.as_str()) {
            return Err(format!("Unknown AI tool '{}'. Valid tools: {}, none", t, AI_TOOLS.join(", ")));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    Ok(out)
}

const MENU: &[(&str, &str)] = &[
    ("1", "claude"),
    ("2", "cursor"),
    ("3", "windsurf"),
    ("4", "copilot"),
    ("5", "opencode"),
    ("6", "codex"),
];

fn select_tools_interactively() -> Vec<String> {
    header("Which AI tool do you use?");
    for (n, t) in MENU {
        println!("  {}) {}", n, tool_display_name(t));
    }
    println!("  7) Multiple (select next)");
    println!("  8) None / skip");
    let choice = prompt_line("Choice [1-8] (default: 1): ").unwrap_or_default();
    let choice = if choice.is_empty() { "1".to_string() } else { choice };
    let pick = |c: &str| MENU.iter().find(|(n, _)| *n == c).map(|(_, t)| t.to_string());
    match choice.as_str() {
        "7" => {
            let multi = prompt_line("Enter tool numbers separated by spaces (e.g. 1 2 5): ").unwrap_or_default();
            let mut out = Vec::new();
            for c in multi.split_whitespace() {
                if let Some(t) = pick(c) {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
            out
        }
        "8" => {
            info("Skipped tool config: .knobyte/AGENTS.md works with any tool that can read files");
            Vec::new()
        }
        c => match pick(c) {
            Some(t) => vec![t],
            None => {
                warn("Unknown choice, skipping tool config");
                Vec::new()
            }
        },
    }
}

/// Exact git commands for the commit checkpoint.
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
    for (tool, path) in [
        ("cursor", ".cursorrules"),
        ("windsurf", ".windsurfrules"),
        ("copilot", ".github/copilot-instructions.md"),
        ("opencode", ".opencode/opencode.json"),
    ] {
        if has(tool) {
            paths.push(path.into());
        }
    }
    if config.project_root.join(".gitignore").exists() {
        paths.push(".gitignore".into());
    }
    paths
}

const CHECKPOINT_MESSAGE: &str = "chore: initialize Knobyte project memory";

fn print_commit_commands(paths: &[String]) {
    println!("    git status --short");
    println!("    git add -- {}", paths.join(" "));
    println!("    git commit -m \"{}\"", CHECKPOINT_MESSAGE);
}

/// Stage exactly `paths` and commit them. Never pushes.
pub fn create_commit_checkpoint(project_root: &Path, paths: &[String]) -> Result<String, String> {
    let existing: Vec<&String> = paths.iter().filter(|p| project_root.join(p).exists()).collect();
    let add = Command::new("git")
        .arg("add")
        .arg("--")
        .args(existing.iter().map(|s| s.as_str()))
        .current_dir(project_root)
        .output()
        .map_err(|e| e.to_string())?;
    if !add.status.success() {
        return Err(String::from_utf8_lossy(&add.stderr).trim().to_string());
    }
    let commit = Command::new("git")
        .args(["commit", "-m", CHECKPOINT_MESSAGE, "--"])
        .args(existing.iter().map(|s| s.as_str()))
        .current_dir(project_root)
        .output()
        .map_err(|e| e.to_string())?;
    if !commit.status.success() {
        let msg = format!("{}{}", String::from_utf8_lossy(&commit.stdout), String::from_utf8_lossy(&commit.stderr));
        return Err(msg.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&commit.stdout).lines().next().unwrap_or("").to_string())
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

fn build_graph(config: &KnobyteConfig) -> Result<(), String> {
    use crate::graph::{rebuild_graph, scan_corpus, CorpusPolicy};
    let root = &config.project_root;
    let policy = CorpusPolicy::for_project(root);
    let scan = scan_corpus(root, &policy).map_err(|e| e.to_string())?;
    let progress = crate::progress::IndexProgressBar::new(scan.total_bytes, scan.files.len(), true);
    let res = rebuild_graph(&config.graph_db_path(), root, &scan, Some(&progress)).map_err(|e| e.to_string());
    progress.finish_and_clear();
    res.map(|_| ())
}

/// Run the setup flow.
pub fn run_setup_flow(config: &KnobyteConfig, opts: &SetupFlowOptions) -> Result<SetupFlowResult, String> {
    let mode = resolve_setup_mode(config, opts.mode.as_deref())?;
    let dry = opts.dry_run;
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
    // A populated scaffold that is not finishing a paused setup: leave tracked content alone.
    let rerun = populated_at_start && !pending_marker(config).exists();
    // Agent files (anchors, skills, aiTools) are rewritten on a re-run only when asked for.
    let write_agent_files = !rerun || opts.tools.is_some();
    match (mode.as_str(), state) {
        ("agent-memory", _) => info("Detected: agent-memory workspace (persistent-agent operational memory)"),
        (_, ProjectState::Existing) => info("Detected: existing codebase with source files; populate the scaffold from code"),
        (_, ProjectState::Fresh) => info("Detected: fresh project (no source files yet); populate the scaffold from intent"),
        (_, ProjectState::Partial) => info("Detected: existing codebase with a populated scaffold; preserve authored files and finish setup"),
    }
    if rerun {
        info("The scaffold is already populated: tracked files stay as they are; setup refreshes local state (graph, wiki index)");
    }

    // 2. Scaffold.
    header("Creating the .knobyte/ scaffold...");
    // On a re-run only missing directories are created; file changes are reported.
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
    for a in report.actions.iter().filter(|a| a.action != "create_dir") {
        if rerun {
            held_back.push(format!("{} ({})", display(&a.path), a.detail));
            continue;
        }
        let verb = match (dry, a.action.as_str()) {
            (true, "modify_file") => "Would modify",
            (true, _) => "Would create",
            (false, "modify_file") => "Modified",
            (false, _) => "Created",
        };
        ok(format!("{} {} ({})", verb, display(&a.path), a.detail));
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
    let saved = load_ai_tools(&config.scaffold_root);
    let tools: Vec<String> = if let Some(t) = &opts.tools {
        t.clone()
    } else if let Some(t) = saved.clone() {
        info(format!(
            "Using configured AI tools: {}",
            if t.is_empty() { "none".to_string() } else { t.iter().map(|x| tool_display_name(x)).collect::<Vec<_>>().join(", ") }
        ));
        t
    } else if opts.interactive && is_interactive() {
        select_tools_interactively()
    } else {
        info("No AI tool selected; defaulting to Claude Code (pass --tools to choose, --tools none to skip)");
        vec!["claude".to_string()]
    };
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

    // 4. Agent skills + managed instruction blocks (Claude Code / Codex).
    let clients: Vec<&str> = tools.iter().map(String::as_str).filter(|t| *t == "claude" || *t == "codex").collect();
    if !clients.is_empty() {
        header("Installing Knobyte agent skills...");
        let assets = sync_agent_assets(
            config,
            &clients,
            SkillSyncOptions { dry_run: agent_dry, check_ignored: mode == "code-repo", backup_conflicts: opts.backup_skills },
        )?;
        for a in &assets.actions {
            match a.action.as_str() {
                "conflict" => {}
                "unchanged" => info(&a.message),
                _ if !write_agent_files => {
                    agent_files_pending = true;
                    info(format!("(not applied) {}", a.message))
                }
                _ => ok(if dry { format!("(dry run) {}", a.message) } else { a.message.clone() }),
            }
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
        if write_agent_files {
            info("Start a new agent session so the new skills and project instructions are loaded.");
        }
    }
    if agent_files_pending {
        info(format!(
            "Agent files were left unchanged. Run `knobyte setup --tools {}` (or `knobyte skills sync`) to write them.",
            if tools.is_empty() { "none".to_string() } else { tools.join(",") }
        ));
    }

    // 5. Scan.
    let mut brief_json: Option<String> = None;
    if mode != "agent-memory" && state != ProjectState::Fresh {
        info("Scanning codebase...");
        let brief = crate::scanner::scan(&config.project_root);
        brief_json = serde_json::to_string_pretty(&brief).ok();
        ok("Pre-analysis complete; the agent will reason from the brief instead of exploring");
    }

    // 6. Code graph.
    if mode != "agent-memory" && !dry && !opts.skip_graph {
        info("Building code graph...");
        match build_graph(config) {
            Ok(()) => ok("Code graph ready"),
            Err(e) => return Err(format!("Code graph setup failed: {}. Fix the problem and rerun knobyte setup.", e)),
        }
    }

    let prompt = build_population_prompt(&mode, state, brief_json.as_deref());

    if dry {
        header("Would run population (dry run; skipping)");
        ok("Done (dry run).");
        return Ok(SetupFlowResult { mode, stage: SetupStage::DryRun, tools, prompt: Some(prompt), anchor_notes });
    }

    // 7. Population.
    let mut populated = populated_at_start;
    if populated {
        header("Finishing setup from the existing populated scaffold...");
    } else {
        header("Populating the scaffold...");
        let candidate = match &opts.agent {
            Some(a) => {
                let tool = AgentTool::parse(a).ok_or_else(|| format!("Unknown agent '{}'. Use claude or codex.", a))?;
                find_on_path(tool.program(), None).map(|_| tool)
            }
            None => select_agent(&tools, None),
        };
        let interactive = opts.interactive && is_interactive();
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
                    let go = opts.launch_agent || confirm(&format!("Launch {} to populate the scaffold now?", tool.display_name()), true);
                    if go {
                        let outcome = run_agent(tool, &prompt, &launch_opts, &mut |ev| print_event(ev));
                        match outcome.failure {
                            None => {
                                ok(format!("{} finished the population session", tool.display_name()));
                                populated = is_scaffold_populated(&config.scaffold_root);
                                if !populated {
                                    warn(format!(
                                        "The agent exited successfully, but these files still need population: {}",
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
                info("No selected Claude Code or Codex CLI is installed on PATH; falling back to the prompt.")
            }
            _ => {}
        }

        if !populated {
            header("Almost done. One more step: populate the scaffold.");
            info("Paste the prompt below into your AI tool. The agent will read your codebase and fill every scaffold file.");
            print_prompt_for_paste(&prompt);
            if interactive {
                info("After the agent finishes populating, return here to finish setup.");
                if confirm("Has population finished?", false) {
                    populated = is_scaffold_populated(&config.scaffold_root);
                    if !populated {
                        warn(format!(
                            "These files still carry the populate marker: {}",
                            unpopulated_files(&config.scaffold_root).join(", ")
                        ));
                    }
                }
            }
        }
    }

    if !populated {
        // Remember that the next run finishes this fresh setup (baselines included).
        let _ = std::fs::create_dir_all(config.local_dir())
            .and_then(|_| std::fs::write(pending_marker(config), "population pending\n"));
        info("Setup paused at population. After the agent finishes, rerun `knobyte setup` to capture groundings and build the wiki index.");
        print_anchor_notes(&anchor_notes);
        return Ok(SetupFlowResult { mode, stage: SetupStage::NeedsPopulation, tools, prompt: Some(prompt), anchor_notes });
    }

    // 8. Finalize.
    header("Finalizing...");
    if rerun {
        let f = crate::wiki::finalize::finalize_wiki_with(
            config,
            &crate::wiki::finalize::FinalizeOptions {
                capture_baselines: opts.capture_baselines,
                ..crate::wiki::finalize::FinalizeOptions::read_only()
            },
        );
        if !f.ready {
            return Err(f.failure_message());
        }
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
        if opts.capture_baselines {
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
            "Wiki index rebuilt with {} entit{}; tracked scaffold files were not modified",
            f.indexed_entities,
            if f.indexed_entities == 1 { "y" } else { "ies" }
        ));
        print_anchor_notes(&anchor_notes);
        return Ok(SetupFlowResult { mode, stage: SetupStage::Ready, tools, prompt: None, anchor_notes });
    }
    let (captured, entities) = finalize_setup(config)?;
    let _ = std::fs::remove_file(pending_marker(config));
    if captured > 0 {
        ok(format!("Captured {} grounding baseline(s)", captured));
    } else {
        info("No authored grounding baselines needed capture");
    }
    ok(format!("Wiki ready with {} indexed entit{}", entities, if entities == 1 { "y" } else { "ies" }));

    // 9. Commit checkpoint (user-confirmed).
    if mode == "code-repo" && has_git {
        header("Commit checkpoint");
        let paths = commit_checkpoint_paths(config, &tools);
        info("Review the scoped files, then commit them:");
        print_commit_commands(&paths);
        let go = opts.commit || (opts.interactive && confirm("Create this commit now (staging only the paths above)?", false));
        if go {
            match create_commit_checkpoint(&config.project_root, &paths) {
                Ok(line) => ok(format!("Committed: {}", line)),
                Err(e) => warn(format!("Commit checkpoint failed: {}", e)),
            }
        } else {
            info("Knobyte did not stage or commit anything.");
        }
    }

    print_anchor_notes(&anchor_notes);
    header("What's next");
    info("Start a fresh agent session and ask: \"Read .knobyte/ROUTER.md and tell me what you know about this project.\"");
    println!("    knobyte check            Drift score: are scaffold files still accurate?");
    println!("    knobyte sync             Repair drift with an agent or targeted prompts");
    println!("    knobyte watch            Check drift after every commit");
    println!("    knobyte hub              Open the Project Hub");
    Ok(SetupFlowResult { mode, stage: SetupStage::Ready, tools, prompt: None, anchor_notes })
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
        assert!(parse_tool_list("none").unwrap().is_empty());
        assert!(parse_tool_list("vim").is_err());
    }
}
