//! CLI rendering for the agent protocol v3 commands (`graph scope` / `query` / `get`,
//! `impact`) and `graph ground` retro-grounding.

use colored::Colorize;

use crate::config::{KnobyteConfig, DEFAULT_SCAFFOLD_DIR};
use crate::graph::engine::{GraphEngine, ImpactOptions};
use crate::graph::read::{ReadGate, Unavailable};

/// Budget and detail controls shared by the agent-facing graph commands.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct AgentFlags {
    /// Detail level: minimal, standard or source
    #[arg(long)]
    pub detail: Option<String>,
    /// Maximum nodes to return
    #[arg(long = "max-nodes")]
    pub max_nodes: Option<usize>,
    /// Maximum source files to return (scope)
    #[arg(long = "max-files")]
    pub max_files: Option<usize>,
    /// Maximum directed flow steps (scope)
    #[arg(long = "max-flow-steps")]
    pub max_flow_steps: Option<usize>,
    /// Hard output token ceiling (estimated, 4 characters per token)
    #[arg(long = "max-output-tokens")]
    pub max_output_tokens: Option<usize>,
    /// Per-node source line cap (with --detail source)
    #[arg(long = "max-source-lines")]
    pub max_source_lines: Option<usize>,
    /// Attach body hashes and serialized MinHash fingerprints to facts (grounding workflow)
    #[arg(long)]
    pub fingerprint: bool,
}

impl AgentFlags {
    pub fn to_input(&self) -> Result<crate::graph::protocol::AgentOptionsInput, String> {
        let detail = match &self.detail {
            Some(d) => Some(
                crate::graph::protocol::DetailLevel::parse(d)
                    .ok_or_else(|| format!("Unknown --detail '{}'. Use minimal, standard or source.", d))?,
            ),
            None => None,
        };
        Ok(crate::graph::protocol::AgentOptionsInput {
            detail,
            max_nodes: self.max_nodes,
            max_files: self.max_files,
            max_flow_steps: self.max_flow_steps,
            max_output_tokens: self.max_output_tokens,
            max_source_lines: self.max_source_lines,
            depth: None,
            fingerprint: self.fingerprint,
        })
    }

    /// True when any agent control was given (selects protocol output).
    pub fn any_set(&self) -> bool {
        self.to_input().map(|i| i.any_set()).unwrap_or(true)
    }
}

fn print_records(records: &[serde_json::Value]) {
    print!("{}", crate::graph::protocol::to_jsonl(records));
}

fn records_exit(records: &[serde_json::Value]) -> i32 {
    match records.last() {
        Some(r) if r["type"] == "summary" => 0,
        _ => 1,
    }
}

fn print_unavailable(u: &Unavailable, machine: bool) {
    if machine {
        println!("{}", u.record());
    } else {
        eprintln!("{} {}", u.reason_code.red().bold(), u.message);
        if let Some(c) = &u.recovery_command {
            eprintln!("Next: {}", c);
        }
    }
}

/// Inspect freshness and open the graph strictly read-only. A tolerant gate (scope) refuses
/// only an unusable store. Prints the refusal and returns `None` when the read is refused.
fn open_read(config: &KnobyteConfig, machine: bool, tolerant: bool) -> Option<(GraphEngine, ReadGate)> {
    let db = config.graph_db_path();
    let opened = ReadGate::open_session(&db, &config.project_root, tolerant);
    match opened {
        Ok(v) => Some(v),
        Err(u) => {
            print_unavailable(&u, machine);
            None
        }
    }
}

/// Open the graph for a command that writes baselines (`graph ground`): never creates it.
fn open_engine(config: &KnobyteConfig, machine: bool) -> Option<GraphEngine> {
    let db = config.graph_db_path();
    if let Err(u) = ReadGate::inspect_tolerant(&db, &config.project_root) {
        print_unavailable(&u, machine);
        return None;
    }
    match GraphEngine::open(&db) {
        Ok(e) => Some(e),
        Err(e) => {
            let m = crate::graph::maintenance::graph_error(&e);
            if machine {
                println!("{}", serde_json::json!({ "type": "error", "code": m.code, "message": m.message }));
            } else {
                eprintln!("{} {}", m.code.red().bold(), m.message);
            }
            None
        }
    }
}

/// Vector hits for `--hybrid` (Cozo HNSW over code nodes).
fn hybrid_hits(config: &KnobyteConfig, task: &str) -> Result<Vec<(String, f64)>, String> {
    let cozo = crate::cozo::CozoEngine::open_configured(config).map_err(|e| e.to_string())?;
    let hits = cozo.vector_search(task, "code", 40).map_err(|e| e.to_string())?;
    Ok(hits.into_iter().map(|m| (m.id, m.score)).collect())
}

/// Knowledge records (`--wiki`) for node ids: wiki entities grounded to them.
pub fn knowledge_records(config: &KnobyteConfig, ids: &[String]) -> Vec<serde_json::Value> {
    if !config.wiki_db_path().exists() {
        return Vec::new();
    }
    let Ok(index) = crate::wiki::index::WikiIndex::open(&config.wiki_db_path()) else {
        return Vec::new();
    };
    let Ok(page) = index.for_code_many(ids, Some(20)) else {
        return Vec::new();
    };
    page.items
        .iter()
        .map(|h| {
            serde_json::json!({
                "type": "knowledge",
                "id": h.entity.id,
                "entityType": h.entity.entity_type,
                "title": h.entity.title,
                "status": h.entity.status,
                "health": h.grounding_health,
                "file": h.entity.file,
                "startLine": h.entity.start_line,
                "matchedNodes": [h.query],
                "grounding": h.grounding,
                "summary": h.entity.summary,
            })
        })
        .collect()
}

/// Protocol records for `graph scope` with the optional Knobyte providers.
pub fn scope_records(
    config: &KnobyteConfig,
    engine: &GraphEngine,
    task: &str,
    input: &crate::graph::protocol::AgentOptionsInput,
    wiki: bool,
    hybrid: bool,
) -> Vec<serde_json::Value> {
    let mut extras = crate::graph::agent::ScopeExtras::default();
    if hybrid {
        match hybrid_hits(config, task) {
            Ok(h) => extras.vector_hits = h,
            Err(e) => extras
                .warnings
                .push(format!("Hybrid re-rank unavailable ({}); lexical ranking only.", e)),
        }
    }
    let knowledge = |ids: &[String]| knowledge_records(config, ids);
    if wiki {
        extras.knowledge_for = Some(&knowledge);
    }
    crate::graph::agent::run_scope(engine, &config.project_root, task, input, &extras)
}

/// `knobyte graph scope <task>`.
#[allow(clippy::too_many_arguments)]
pub fn run_scope_cmd(
    config: &KnobyteConfig,
    task: &str,
    flags: &AgentFlags,
    wiki: bool,
    hybrid: bool,
    json: bool,
    jsonl: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let input = flags.to_input()?;
    let Some((engine, _gate)) = open_read(config, jsonl || json, true) else { return Ok(1) };
    let records = scope_records(config, &engine, task, &input, wiki, hybrid);
    if jsonl {
        print_records(&records);
    } else if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else {
        print_scope_human(&records);
    }
    Ok(records_exit(&records))
}

fn print_scope_human(records: &[serde_json::Value]) {
    for r in records {
        match r["type"].as_str().unwrap_or("") {
            "error" => eprintln!(
                "{} {}",
                r["code"].as_str().unwrap_or("ERROR").red().bold(),
                r["message"].as_str().unwrap_or("")
            ),
            "source" => {
                println!("{} {}", "==".dimmed(), r["filePath"].as_str().unwrap_or("").bold());
                for range in r["ranges"].as_array().into_iter().flatten() {
                    println!("{}", range["content"].as_str().unwrap_or(""));
                    if range["truncated"].as_bool() == Some(true) {
                        println!("{}", "   ... (truncated)".dimmed());
                    }
                }
            }
            "flow" => {
                let names: Vec<String> = r["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|n| n["name"].as_str().unwrap_or("").to_string())
                    .collect();
                println!("{} {}", "flow:".cyan(), names.join(" -> "));
            }
            "fact" => println!(
                "{} {} ({}:{})",
                r["kind"].as_str().unwrap_or("").cyan(),
                r["qualifiedName"].as_str().or(r["name"].as_str()).unwrap_or("").bold(),
                r["filePath"].as_str().unwrap_or(""),
                r["lineStart"]
            ),
            "knowledge" => println!(
                "{} {} ({})",
                "knowledge:".magenta(),
                r["title"].as_str().unwrap_or(""),
                r["file"].as_str().unwrap_or("")
            ),
            "summary" => {
                println!(
                    "{} status {}, evidence {}, {} node(s), {} file(s){}",
                    "--".dimmed(),
                    r["status"].as_str().unwrap_or(""),
                    r["evidenceStrength"].as_str().unwrap_or(""),
                    r["returnedNodes"],
                    r["returnedFiles"].as_array().map(|a| a.len()).unwrap_or(0),
                    if r["truncated"].as_bool() == Some(true) { ", truncated" } else { "" }
                );
                for w in r["warnings"].as_array().into_iter().flatten() {
                    println!("{} {}", "[warn]".yellow(), w.as_str().unwrap_or(""));
                }
            }
            _ => {}
        }
    }
}

/// `knobyte graph query <relation> <target> --jsonl` (protocol v3).
pub fn run_query_protocol(
    config: &KnobyteConfig,
    relation: &str,
    target: &str,
    flags: &AgentFlags,
) -> Result<i32, Box<dyn std::error::Error>> {
    let input = flags.to_input()?;
    if !crate::graph::agent::QUERY_RELATIONS.contains(&relation) {
        let records = vec![serde_json::json!({
            "type": "error",
            "code": "INVALID_QUERY",
            "relation": relation,
            "expected": crate::graph::agent::QUERY_RELATIONS,
        })];
        print_records(&records);
        return Ok(1);
    }
    let Some((engine, gate)) = open_read(config, true, false) else { return Ok(1) };
    let records =
        crate::graph::agent::run_query_gated(&engine, &gate, &config.project_root, relation, target, &input);
    print_records(&records);
    Ok(records_exit(&records))
}

/// `knobyte graph get <ids> --jsonl` (protocol v3).
pub fn run_get_protocol(
    config: &KnobyteConfig,
    ids: &[String],
    flags: &AgentFlags,
) -> Result<i32, Box<dyn std::error::Error>> {
    let input = flags.to_input()?;
    let Some((engine, gate)) = open_read(config, true, false) else { return Ok(1) };
    let records = crate::graph::agent::run_get_gated(&engine, &gate, &config.project_root, ids, &input);
    print_records(&records);
    Ok(records_exit(&records))
}

/// `knobyte impact <target> --jsonl` (protocol v3).
pub fn run_impact_protocol(
    config: &KnobyteConfig,
    target: &str,
    opts: ImpactOptions,
    flags: &AgentFlags,
) -> Result<i32, Box<dyn std::error::Error>> {
    let input = flags.to_input()?;
    let Some((engine, gate)) = open_read(config, true, false) else { return Ok(1) };
    let records = crate::graph::agent::run_impact_gated(
        &engine,
        &gate,
        &config.project_root,
        &config.scaffold_root,
        target,
        opts,
        &input,
    );
    print_records(&records);
    Ok(records_exit(&records))
}

/// Modes of `knobyte graph ground`.
#[derive(Debug, Clone, Default)]
pub struct GroundMode {
    /// Re-baseline existing groundings (the default when no other mode is given).
    pub rebaseline: bool,
    /// Show proposed groundings for ungrounded entities without writing.
    pub dry_run: bool,
    /// Write proposed groundings into the documents, then re-baseline.
    pub apply: bool,
    /// Hand retro-grounding to a coding agent (prints the prompt when none can be launched).
    pub agent: bool,
    /// Launch the agent without asking (the explicit consent `setup`/`sync` use).
    pub launch_agent: bool,
    pub per_entity: usize,
    pub json: bool,
}

/// `knobyte graph ground`.
pub fn run_ground(config: &KnobyteConfig, mode: &GroundMode) -> Result<i32, Box<dyn std::error::Error>> {
    let Some(engine) = open_engine(config, mode.json) else { return Ok(1) };
    if mode.agent {
        let dir = config
            .scaffold_root
            .strip_prefix(&config.project_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| DEFAULT_SCAFFOLD_DIR.to_string());
        let prompt = crate::graph::ground::ground_prompt(&dir);
        if mode.dry_run {
            println!("{}", prompt);
            return Ok(0);
        }
        let configured = crate::config::load_ai_tools(&config.scaffold_root).unwrap_or_default();
        let tool = crate::agent::select_agent(&configured, None)
            .or_else(|| crate::agent::installed_agents(None).into_iter().next());
        // Same consent rules as setup/sync: `--agent` alone never launches without showing the
        // command and asking; `--launch-agent` is the explicit consent for non-interactive use.
        let interactive = crate::agent::is_interactive();
        let permitted = crate::agent::launch_permitted(interactive, mode.launch_agent);
        match (tool, permitted) {
            (Some(tool), Ok(())) => {
                let opts = crate::agent::LaunchOptions {
                    cwd: config.project_root.clone(),
                    private_dir: config.local_dir(),
                    timeout: None,
                    path_env: None,
                    allow_non_git: !config.project_root.join(".git").exists(),
                };
                crate::agent::print_launch_preview(
                    tool,
                    &crate::agent::preview_command(tool, &opts),
                    &config.project_root,
                );
                if !mode.launch_agent
                    && !crate::agent::confirm(
                        &format!("Launch {} for agent-led retro-grounding?", tool.display_name()),
                        false,
                    )
                {
                    println!("Not launched. Give this prompt to your agent, then run `knobyte graph ground --rebaseline`:\n");
                    println!("{}", prompt);
                    return Ok(0);
                }
                println!("Launching {} for agent-led retro-grounding...", tool.display_name());
                let outcome = crate::agent::run_agent(tool, &prompt, &opts, &mut |_| {});
                if !outcome.completed {
                    eprintln!("{} the agent did not complete; baselines were not captured.", "[warn]".yellow());
                    return Ok(1);
                }
            }
            _ => {
                println!(
                    "No agent CLI can be launched here. Give this prompt to your agent, then run `knobyte graph ground --rebaseline`:\n"
                );
                println!("{}", prompt);
                return Ok(0);
            }
        }
        let count = engine.ground_all(&config.project_root)?;
        println!("{} Captured {} grounding baseline(s).", "[ok]".green().bold(), count);
        return Ok(0);
    }
    if mode.dry_run || mode.apply {
        let proposals = crate::graph::ground::propose_groundings(
            engine.connection(),
            &config.scaffold_root,
            mode.per_entity.max(1),
        );
        let applied = if mode.apply {
            crate::graph::ground::apply_groundings(&config.scaffold_root, &proposals)?
        } else {
            0
        };
        let baselined = if mode.apply {
            Some(engine.ground_all(&config.project_root)?)
        } else {
            None
        };
        if mode.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dryRun": !mode.apply,
                    "proposals": proposals,
                    "documentsChanged": applied,
                    "baselined": baselined,
                }))?
            );
            return Ok(0);
        }
        if proposals.is_empty() {
            println!(
                "{} No ungrounded entity has strong enough graph evidence to propose a grounding.",
                "[ok]".green().bold()
            );
        }
        for p in &proposals {
            println!("{} {} ({})", "*".cyan(), p.title.bold(), p.doc);
            for r in &p.refs {
                println!("    + {}  [{}]", r.reference, r.reasons.join(", ").dimmed());
            }
        }
        if mode.apply {
            println!(
                "{} Wrote grounds_to into {} document(s); {} grounding baseline(s) captured.",
                "[ok]".green().bold(),
                applied,
                baselined.unwrap_or(0)
            );
        } else if !proposals.is_empty() {
            println!("Dry run: nothing written. Review, then run `knobyte graph ground --apply`.");
        }
        return Ok(0);
    }
    let count = engine.ground_all(&config.project_root)?;
    if mode.json {
        println!("{}", serde_json::json!({ "baselined": count }));
    } else {
        println!("{} Re-baselined {} grounded references.", "[ok]".green().bold(), count);
    }
    Ok(0)
}
