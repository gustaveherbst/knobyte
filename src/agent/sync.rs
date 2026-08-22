//! `knobyte sync`: relocate moved groundings, then loop brief -> agent (or printed prompt) ->
//! re-check -> score delta -> baseline capture on accept.

use std::path::Path;
use std::time::Duration;

use colored::Colorize;

use crate::agent::{
    confirm, installed_agents, is_interactive, launch_permitted, preview_command, print_event,
    print_launch_preview, print_prompt_for_paste, prompt_line, run_agent, select_agent, AgentTool,
    LaunchOptions,
};
use crate::config::{load_ai_tools, KnobyteConfig};
use crate::drift::{build_sync_brief_with, codes, run_drift_check, sync_groundings, DriftReport, SyncBriefOptions};
use crate::graph::grounding::capture_baselines;
use crate::graph::GraphEngine;

/// Interactive sync sessions keep a bounded default.
pub const SYNC_AGENT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub include_warnings: bool,
    /// Launch the agent without the interactive menu (explicit consent).
    pub launch_agent: bool,
    /// Agent to use (`claude` / `codex`); defaults to the configured tools.
    pub agent: Option<String>,
    /// Print the repair prompt and exit (never launch).
    pub print_prompt: bool,
    /// Capture grounding baselines for repaired files without asking.
    pub accept: bool,
    /// Maximum repair cycles (default 3).
    pub max_cycles: Option<usize>,
    /// Skip the grounding relocation step (`check --fix` plans and applies it, with consent,
    /// before handing remaining errors to this flow).
    pub skip_relocation: bool,
}

fn relevant_issue_count(report: &DriftReport) -> usize {
    report
        .issues
        .iter()
        .filter(|i| i.code != codes::GROUNDING_MOVED_BY_NEIGHBORS)
        .count()
}

/// Capture grounding baselines for the given project-relative (or scaffold-relative) scaffold
/// files: cached in graph.db and committed into the markdown. Returns the number of references
/// baselined.
pub fn capture_baselines_for(config: &KnobyteConfig, files: &[String]) -> Result<usize, String> {
    let db = config.graph_db_path();
    if !db.exists() {
        return Ok(0);
    }
    let engine = GraphEngine::open(&db).map_err(|e| e.to_string())?;
    let prefix = config
        .scaffold_root
        .strip_prefix(&config.project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let include = |doc: &str| {
        let project_rel = if prefix.is_empty() { doc.to_string() } else { format!("{}/{}", prefix, doc) };
        files.iter().any(|f| f == &project_rel || f == doc)
    };
    capture_baselines(engine.connection(), &config.project_root, &config.scaffold_root, &include)
        .map_err(|e| e.to_string())
}

fn pick_agent(config: &KnobyteConfig, requested: Option<&str>) -> Result<Option<AgentTool>, String> {
    if let Some(r) = requested {
        let tool = AgentTool::parse(r).ok_or_else(|| format!("Unknown agent '{}'. Use claude or codex.", r))?;
        return Ok(crate::agent::find_on_path(tool.program(), None).map(|_| tool));
    }
    let configured = load_ai_tools(&config.scaffold_root).unwrap_or_default();
    if let Some(t) = select_agent(&configured, None) {
        return Ok(Some(t));
    }
    let installed = installed_agents(None);
    match installed.len() {
        0 => Ok(None),
        1 => Ok(Some(installed[0])),
        _ if is_interactive() => {
            println!("{}", "No agent CLI configured, but several are installed:".yellow());
            for (i, t) in installed.iter().enumerate() {
                println!("  {}) {}", i + 1, t.display_name());
            }
            let choice = prompt_line(&format!("Which one should fix these? [1-{}] (default: 1): ", installed.len()))
                .unwrap_or_default();
            let idx = choice.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= installed.len()).unwrap_or(1);
            Ok(Some(installed[idx - 1]))
        }
        _ => Ok(Some(installed[0])),
    }
}

/// Run the sync loop. Returns the process exit code (0 when no errors remain).
pub fn run_sync(config: &KnobyteConfig, opts: &SyncOptions) -> Result<i32, String> {
    // Relocate grounding anchors whose code symbols moved (deterministic, no agent).
    let relocations = if opts.skip_relocation {
        crate::drift::SyncResult {
            proposals: Vec::new(),
            relocated_count: 0,
            dry_run: opts.dry_run,
            success: true,
            message: String::new(),
            skipped: None,
        }
    } else {
        sync_groundings(config, opts.dry_run)?
    };
    if let Some(reason) = &relocations.skipped {
        println!(
            "{} Grounding relocation skipped: the code graph is not fresh ({}).",
            "[info]".cyan().bold(),
            reason
        );
    }
    if !relocations.proposals.is_empty() {
        println!("{}", "=== Grounding Anchor Relocations ===".bold());
        for prop in &relocations.proposals {
            println!(
                "  - [{}] {}: {} -> {} (confidence: {:.0}%)",
                prop.scaffold_file.cyan(),
                prop.symbol_name.bold(),
                prop.old_file.as_deref().unwrap_or(&prop.old_node_id).dimmed(),
                prop.new_file.green(),
                prop.confidence * 100.0
            );
        }
        if opts.dry_run {
            println!("{} {} anchor(s) eligible for relocation (dry run).", "[info]".cyan().bold(), relocations.proposals.len());
        } else {
            println!("{} Relocated {} grounding anchor(s).", "[ok]".green().bold(), relocations.relocated_count);
        }
        println!();
    }

    let interactive = is_interactive();
    let max_cycles = opts.max_cycles.unwrap_or(3).max(1);
    let mut agent: Option<AgentTool> = None;
    let mut mode_chosen = false;

    for cycle in 1..=max_cycles {
        println!(
            "{}",
            if cycle == 1 { "Running drift check...".bold() } else { "Re-checking for remaining drift...".bold() }
        );
        let report = run_drift_check(config);
        if relevant_issue_count(&report) == 0 {
            if report.grounding.unverified > 0 {
                let graph = report.graph.as_ref().map(|g| g.summary()).unwrap_or_default();
                println!(
                    "{} No drift found in what could be checked, but {} grounding(s) could not be verified ({}).",
                    "[info]".cyan().bold(),
                    report.grounding.unverified,
                    graph
                );
                return Ok(0);
            }
            println!("{} No drift detected. Everything is in sync.", "[ok]".green().bold());
            return Ok(0);
        }
        println!(
            "{}",
            format!("Found {} issue(s) (score: {:.0}/100)", relevant_issue_count(&report), report.score).yellow()
        );

        let brief = build_sync_brief_with(config, &report, SyncBriefOptions { include_warnings: opts.include_warnings });
        if brief.is_empty() {
            println!("{} No errors found. Only warnings remain (use --warnings to include them).", "[ok]".green().bold());
            if report.grounding.unverified > 0 {
                println!(
                    "{} {} grounding(s) could not be verified; see `knobyte check`.",
                    "[info]".cyan().bold(),
                    report.grounding.unverified
                );
            }
            return Ok(0);
        }
        println!("\n{} file(s) need attention:\n", brief.targets.len());
        for t in &brief.targets {
            println!("  {} ({} errors, {} warnings)", t.file, t.errors(), t.warnings());
        }

        if opts.dry_run {
            println!("{}", "\n--dry-run: showing the repair prompt without executing\n".dimmed());
            println!("{}", brief.prompt);
            // A dry run only reports, so it succeeds even when drift remains
            // (`knobyte check` is the gate that fails on drift).
            return Ok(0);
        }

        if !mode_chosen {
            mode_chosen = true;
            if opts.print_prompt {
                print_prompt_for_paste(&brief.prompt);
                return Ok(if report.count("error") > 0 { 1 } else { 0 });
            }
            let candidate = pick_agent(config, opts.agent.as_deref())?;
            let gate = launch_permitted(interactive, opts.launch_agent);
            match (candidate, gate) {
                (Some(tool), Ok(())) => {
                    let launch_opts = launch_options(config);
                    print_launch_preview(tool, &preview_command(tool, &launch_opts), &config.project_root);
                    if opts.launch_agent {
                        agent = Some(tool);
                    } else {
                        println!("\n{}", "How should we fix these?".bold());
                        println!("  1) Launch {} to repair them now (default)", tool.display_name());
                        println!("  2) Show the prompt; I'll paste it into my agent");
                        println!("  3) Exit");
                        match prompt_line("Choice [1-3] (default: 1): ").as_deref() {
                            Some("") | Some("1") => agent = Some(tool),
                            Some("2") => {
                                print_prompt_for_paste(&brief.prompt);
                                return Ok(1);
                            }
                            _ => {
                                println!("{}", "Exiting. Run knobyte sync again anytime.".dimmed());
                                return Ok(1);
                            }
                        }
                    }
                }
                (None, _) => {
                    println!(
                        "{}",
                        "No Claude Code or Codex CLI found on PATH; paste this prompt into your agent:".yellow()
                    );
                    print_prompt_for_paste(&brief.prompt);
                    return Ok(1);
                }
                (Some(_), Err(reason)) => {
                    println!("{} Not launching an agent: {}.", "[info]".cyan().bold(), reason);
                    print_prompt_for_paste(&brief.prompt);
                    return Ok(1);
                }
            }
        }

        let Some(tool) = agent else { return Ok(1) };
        println!(
            "\n{}",
            format!("Sending {} file(s) to {} in one session...", brief.targets.len(), tool.display_name()).bold()
        );
        let mut launch_opts = launch_options(config);
        launch_opts.timeout = Some(SYNC_AGENT_TIMEOUT);
        let outcome = run_agent(tool, &brief.prompt, &launch_opts, &mut |ev| print_event(ev));
        if let Some(f) = outcome.failure {
            println!("{} {}", "[fail]".red().bold(), f.message(tool));
            if f == crate::agent::LaunchFailure::Cancelled {
                return Ok(130);
            }
        }

        let post = run_drift_check(config);
        let delta = post.score - report.score;
        let delta_str = if delta > 0.0 {
            format!("+{:.0}", delta).green()
        } else if delta == 0.0 {
            "+0".yellow()
        } else {
            format!("{:.0}", delta).red()
        };
        println!("\n{}", format!("Drift score: {:.0} -> {:.0}/100 ({})", report.score, post.score, delta_str).bold());

        if outcome.completed {
            let files: Vec<String> = brief.targets.iter().map(|t| t.file.clone()).collect();
            let accept = opts.accept
                || confirm("Accept the repaired files and capture grounding baselines for them?", false);
            if accept {
                match capture_baselines_for(config, &files) {
                    Ok(n) => println!("{} Captured {} grounding baseline(s). Commit and push to share them.", "[ok]".green().bold(), n),
                    Err(e) => println!("{} Grounding baselines were not captured: {}", "[warn]".yellow().bold(), e),
                }
            } else {
                println!("{}", "Existing grounding baselines were preserved.".dimmed());
            }
        }

        let errors = post.count("error");
        let warnings = post.count("warning");
        if errors == 0 && !opts.include_warnings {
            if warnings > 0 {
                println!("{}", format!("{} warning(s) remain (use --warnings to include them).", warnings).dimmed());
            } else {
                println!("{} All issues resolved.", "[ok]".green().bold());
            }
            return Ok(0);
        }
        if post.score >= 100.0 {
            println!("{} Perfect score. All issues resolved.", "[ok]".green().bold());
            return Ok(0);
        }
        if outcome.failure.is_some() {
            return Ok(1);
        }
        let remaining = if opts.include_warnings { errors + warnings } else { errors };
        if cycle == max_cycles {
            println!("{}", format!("{} issue(s) remain after {} cycle(s).", remaining, max_cycles).yellow());
            return Ok(1);
        }
        let again = if opts.launch_agent && !interactive {
            true
        } else {
            confirm(&format!("\n{} issue(s) remain. Run another cycle?", remaining), true)
        };
        if !again {
            println!("{}", "Stopped. Run knobyte sync again anytime.".dimmed());
            return Ok(1);
        }
    }
    Ok(1)
}

fn launch_options(config: &KnobyteConfig) -> LaunchOptions {
    LaunchOptions {
        cwd: config.project_root.clone(),
        private_dir: config.local_dir(),
        timeout: None,
        path_env: None,
        allow_non_git: !Path::new(&config.project_root).join(".git").exists(),
    }
}
