//! `knobyte capabilities --json`: versioned, machine-readable discovery for agents.
//! Installed capabilities with availability, the initialization state of this checkout and the
//! next initialization action, command descriptors (grouped as read / preview / apply), and the
//! shared exit-code contract.

use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::cozo::{embedding_status, EmbeddingStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnavailableReason {
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityItem {
    pub id: String,
    pub installed: bool,
    pub availability: String,
    #[serde(rename = "unavailableReason", default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<UnavailableReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDescriptor {
    pub id: String,
    /// Exact command path without arguments or flags.
    pub path: String,
    /// Copy/paste-safe structured invocation.
    pub usage: String,
    /// `json`, `jsonl`, `text`, `sse` or `stdio`.
    pub output: String,
    /// `read` (no writes), `preview` (plans without writing) or `apply` (writes).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitCode {
    pub code: i32,
    pub name: String,
    pub meaning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextInitializationAction {
    /// `None` when the fix is a manual change.
    pub command: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandsByKind {
    pub read: Vec<CommandDescriptor>,
    pub preview: Vec<CommandDescriptor>,
    pub apply: Vec<CommandDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesReport {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "knobyteVersion")]
    pub knobyte_version: String,
    pub repository: RepositoryInfo,
    pub capabilities: Vec<CapabilityItem>,
    /// Every command descriptor (flat list).
    pub commands: Vec<CommandDescriptor>,
    /// The same descriptors grouped by kind.
    #[serde(rename = "commandsByKind")]
    pub commands_by_kind: CommandsByKind,
    #[serde(rename = "exitCodes")]
    pub exit_codes: Vec<ExitCode>,
    #[serde(rename = "nextInitializationAction")]
    pub next_initialization_action: Option<NextInitializationAction>,
    /// Embedding backend used by vector search (backend, model, dim, model presence).
    pub embedding: EmbeddingStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInfo {
    /// Legacy summary: `ready` or `scaffold_missing`.
    pub state: String,
    /// `not_git_repository`, `scaffold_missing`, `scaffold_incomplete`, `needs_population` or `ready`.
    #[serde(rename = "initializationState")]
    pub initialization_state: String,
    /// Code graph freshness (`fresh`, `stale`, `missing`, ...).
    #[serde(rename = "graphIndexState")]
    pub graph_index_state: String,
    /// `ready` or `missing`.
    #[serde(rename = "wikiIndexState")]
    pub wiki_index_state: String,
    pub root: String,
    #[serde(rename = "scaffoldRoot")]
    pub scaffold_root: String,
    /// The persisted `scaffold_id` from `.knobyte/config.json`; `null` when there is none
    /// (never a freshly invented id).
    #[serde(rename = "scaffoldId")]
    pub scaffold_id: Option<String>,
    #[serde(rename = "aiTools", default, skip_serializing_if = "Option::is_none")]
    pub ai_tools: Option<Vec<String>>,
}

/// (id, kind, usage, output, description). The path is the usage up to the first argument.
const COMMANDS: &[(&str, &str, &str, &str, &str)] = &[
    ("init", "read", "knobyte init --json", "json", "Pre-analysed scanner brief: manifests, entry points, folders, tooling, README"),
    ("setup", "apply", "knobyte setup [--cli] [--tools claude,cursor] [--launch-agent|--no-agent] [--dry-run]", "text", "Create the scaffold, link AI tools, install skills, build the graph, populate and finalize"),
    ("update", "apply", "knobyte update [--dry-run] --json", "json", "Refresh Knobyte-owned files and managed blocks without touching populated content"),
    ("check", "read", "knobyte check --json", "json", "Drift score and issues against the code graph, filesystem and git"),
    ("sync", "apply", "knobyte sync [--dry-run] [--warnings] [--launch-agent|--print-prompt]", "text", "Relocate groundings, then repair drift with an agent or printed prompts"),
    ("graph.status", "read", "knobyte graph status --json", "json", "Graph freshness, counts, coverage and remediation"),
    ("graph.refresh", "apply", "knobyte graph refresh --json", "json", "Incrementally re-extract changed files"),
    ("graph.rebuild", "apply", "knobyte graph rebuild --json", "json", "Full graph rebuild, published atomically"),
    ("graph.query", "read", "knobyte graph query <where-defined|who-calls|what-calls|who-imports> <target> --json", "json", "Exact structural lookup"),
    ("graph.scope", "read", "knobyte graph scope <task...> --json", "json", "Bounded source-backed context for a task"),
    ("graph.get", "read", "knobyte graph get <id...> --source --json", "json", "Nodes by id or grounding reference, with source"),
    ("graph.ground", "apply", "knobyte graph ground", "text", "Re-baseline every grounded reference to the current code"),
    ("graph.repair", "apply", "knobyte graph repair --json", "json", "Repair the graph store in place"),
    ("impact", "read", "knobyte impact <target> --json", "json", "Transitive blast radius and grounded scaffold documents"),
    ("wiki.list", "read", "knobyte wiki list --json", "json", "Wiki entities, bounded"),
    ("wiki.show", "read", "knobyte wiki show <id> --json", "json", "One wiki entity"),
    ("wiki.query", "read", "knobyte wiki query <text...> --json", "json", "Full-text wiki search"),
    ("wiki.validate", "read", "knobyte wiki validate --json", "json", "Wiki diagnostics"),
    ("wiki.apply", "preview", "knobyte wiki apply <operation-file> --json", "json", "Plan a wiki operation (add --apply to write)"),
    ("wiki.rebuild_index", "apply", "knobyte wiki rebuild-index --json", "json", "Rebuild the wiki search index"),
    ("cozo.query", "read", "knobyte cozo query <script> --json", "json", "Read-only Datalog query"),
    ("cozo.search", "read", "knobyte cozo search <query> [--target code|wiki] --json", "json", "HNSW vector search"),
    ("cozo.pagerank", "read", "knobyte cozo pagerank --json", "json", "PageRank over code dependencies"),
    ("cozo.shortest_path", "read", "knobyte cozo shortest-path <start> <target> --json", "json", "Shortest path between code nodes"),
    ("cozo.sync", "apply", "knobyte cozo sync --json", "json", "Synchronize graph.db and wiki.db into CozoDB"),
    ("cozo.model.status", "read", "knobyte cozo model status --json", "json", "Embedding backend and model status"),
    ("cozo.model.pull", "apply", "knobyte cozo model pull [--model <hf-repo>] --json", "json", "Download a Model2Vec model"),
    ("cozo.model.use", "apply", "knobyte cozo model use <hashed|model2vec> --json", "json", "Select the embedding backend"),
    ("member.list", "read", "knobyte member list --json", "json", "Team members"),
    ("member.current", "read", "knobyte member current --json", "json", "Effective identity of this checkout"),
    ("activity.list", "read", "knobyte activity list --json", "json", "Canonical activity history"),
    ("workstream.list", "read", "knobyte workstream list --json", "json", "Team workstreams"),
    ("spec.show", "read", "knobyte spec show <id> --json", "json", "One requirements spec"),
    ("inbox.contract", "read", "knobyte inbox contract --json", "json", "Inbox request-file JSON Schema catalog"),
    ("inbox.target", "read", "knobyte inbox target <id> --json", "json", "Exact revision of a knowledge record for a correction"),
    ("inbox.draft.preview", "preview", "knobyte inbox draft save ... --preview --json", "json", "Preview an Inbox draft"),
    ("inbox.draft.apply", "apply", "knobyte inbox draft save --apply <envelope> --json", "json", "Save the exact previewed Inbox draft (checkout-local)"),
    ("inbox.draft.list", "read", "knobyte inbox draft list --json", "json", "Local Inbox drafts"),
    ("inbox.proposal", "read", "knobyte inbox proposal list --json", "json", "Published proposals"),
    ("relay.contract", "read", "knobyte relay contract --json", "json", "Relay request-file JSON Schema catalog"),
    ("relay.draft.preview", "preview", "knobyte relay draft save ... --preview --json", "json", "Preview a relay draft"),
    ("relay.draft.apply", "apply", "knobyte relay draft save --apply <envelope> --json", "json", "Save the exact previewed relay draft (checkout-local)"),
    ("relay.list", "read", "knobyte relay list --json", "json", "Relays"),
    ("relay.show", "read", "knobyte relay show <id> --json", "json", "One relay"),
    ("log", "apply", "knobyte log <message> [--kind decision|discovery|note|risk|todo]", "text", "Append to the event log"),
    ("logging", "read", "knobyte logging [significant|checkpoints|manual] --json", "json", "Read or set this checkout's advisory agent logging mode"),
    ("timeline", "read", "knobyte timeline --json", "json", "Search event history"),
    ("heartbeat", "read", "knobyte heartbeat --json", "json", "Stale scaffold files, memory retention and temp-file cleanup"),
    ("doctor", "read", "knobyte doctor --json", "json", "Health summary with next steps"),
    ("skills.sync", "apply", "knobyte skills sync [--tool claude|codex] [--dry-run] --json", "json", "Install the official agent skills without clobbering edits"),
    ("pattern.add", "apply", "knobyte pattern add <name>", "text", "Create a pattern and list it in patterns/INDEX.md"),
    ("watch", "apply", "knobyte watch [--uninstall] [--interval <minutes>]", "text", "Post-commit drift hook or periodic heartbeat"),
    ("completion", "read", "knobyte completion <bash|zsh|fish>", "text", "Shell completion script"),
    ("export", "read", "knobyte export [--out <path>]", "text", "Whole scaffold as one Markdown bundle"),
    ("capabilities", "read", "knobyte capabilities --json", "json", "This manifest"),
    ("mcp", "read", "knobyte mcp [--stdio | --http | --sse] [--port 3005]", "sse", "Model Context Protocol server"),
];

fn descriptor(row: &(&str, &str, &str, &str, &str)) -> CommandDescriptor {
    let (id, kind, usage, output, description) = *row;
    let path = usage
        .split_whitespace()
        .take_while(|w| !w.starts_with('<') && !w.starts_with('[') && !w.starts_with('-') && *w != "..." && *w != "|")
        .collect::<Vec<_>>()
        .join(" ");
    CommandDescriptor {
        id: id.to_string(),
        path,
        usage: usage.to_string(),
        output: output.to_string(),
        kind: kind.to_string(),
        description: description.to_string(),
    }
}

/// All command descriptors.
pub fn command_descriptors() -> Vec<CommandDescriptor> {
    COMMANDS.iter().map(descriptor).collect()
}

/// The exit-code contract shared by Knobyte's structured commands.
pub fn exit_codes() -> Vec<ExitCode> {
    [
        (0, "ok", "Success, including exact idempotent replay."),
        (1, "validation", "Validation or command failure (for `check`/`doctor`: drift errors); inspect the problem and diagnostics."),
        (2, "usage", "Arguments, request JSON, or preview-envelope input are invalid."),
        (3, "unavailable", "Repository state or the requested resource is unavailable."),
        (4, "conflict", "A revision, operation, or recovery conflict prevented the action."),
        (5, "refused", "A containment, authorization, or safety policy refused the action."),
        (130, "cancelled", "The user interrupted an agent session (Ctrl-C); its process tree was stopped."),
    ]
    .into_iter()
    .map(|(code, name, meaning)| ExitCode { code, name: name.into(), meaning: meaning.into() })
    .collect()
}

fn reason(code: &str, detail: &str) -> Option<UnavailableReason> {
    Some(UnavailableReason { code: code.into(), detail: detail.into() })
}

pub fn get_capabilities(config: &KnobyteConfig) -> CapabilitiesReport {
    let has_git = crate::config::find_git_root(&config.project_root).is_some();
    let scaffold = config.scaffold_root.is_dir();
    let complete = scaffold
        && config.config_file_path().is_file()
        && config.scaffold_root.join("ROUTER.md").is_file()
        && config.scaffold_root.join("AGENTS.md").is_file();
    // The repository is checked first: without one there is nowhere to set up,
    // so `not_git_repository` wins over `scaffold_missing`. A complete scaffold outside git
    // is still usable in the non-code modes.
    let initialization_state = if !has_git && (!scaffold || config.mode == "code-repo") {
        "not_git_repository"
    } else if !scaffold {
        "scaffold_missing"
    } else if !complete {
        "scaffold_incomplete"
    } else if !crate::setup::is_scaffold_populated(&config.scaffold_root) {
        "needs_population"
    } else {
        "ready"
    };
    let next_initialization_action = match initialization_state {
        "scaffold_missing" | "scaffold_incomplete" => Some(NextInitializationAction {
            command: Some("knobyte setup".into()),
            reason: "The Knobyte scaffold is missing or incomplete.".into(),
        }),
        "needs_population" => Some(NextInitializationAction {
            command: Some("knobyte setup --cli".into()),
            reason: "Scaffold files still carry the populate marker; populate them and finish setup.".into(),
        }),
        "not_git_repository" => Some(NextInitializationAction {
            command: None,
            reason: "No git repository found. Run `git init`, then `knobyte setup`.".into(),
        }),
        _ => None,
    };

    let graph_state = if scaffold {
        crate::drift::inspect_graph(config).status.as_str().to_string()
    } else {
        "missing".to_string()
    };
    let wiki_state = if config.wiki_db_path().exists() { "ready" } else { "missing" };

    let embedding = embedding_status(&config.embedding);
    let available = |ok: bool, r: Option<UnavailableReason>| -> (String, Option<UnavailableReason>) {
        if ok {
            ("available".into(), None)
        } else {
            ("unavailable".into(), r)
        }
    };
    let scaffold_reason = reason("SCAFFOLD_MISSING", "Run `knobyte setup` to create the scaffold.");
    let graph_ok = scaffold && graph_state != "missing" && graph_state != "degraded";
    let graph_reason = reason("GRAPH_UNAVAILABLE", "Build the code graph with `knobyte graph rebuild`.");
    let mut capabilities = Vec::new();
    let mut add = |id: &str, (availability, unavailable_reason): (String, Option<UnavailableReason>)| {
        capabilities.push(CapabilityItem { id: id.into(), installed: true, availability, unavailable_reason });
    };
    add("code_graph", available(graph_ok, graph_reason.clone()));
    add("wiki", available(scaffold, scaffold_reason.clone()));
    add("cozodb_graph", available(true, None));
    add(
        "vector_search",
        if embedding.model_present {
            ("available".into(), None)
        } else {
            ("model_missing".into(), reason("MODEL_MISSING", "Run `knobyte cozo model pull` or `knobyte cozo model use hashed`."))
        },
    );
    add("datalog_engine", available(true, None));
    add("drift_check", available(scaffold, scaffold_reason.clone()));
    add("grounding", available(graph_ok, graph_reason));
    add("team_identity", available(scaffold, scaffold_reason.clone()));
    add("team_relay", available(scaffold, scaffold_reason.clone()));
    add("team_inbox", available(scaffold, scaffold_reason.clone()));
    add("team_workstreams", available(scaffold, scaffold_reason.clone()));
    add("activity", available(scaffold, scaffold_reason.clone()));
    add("agent_skills", available(true, None));
    add("agent_launch", available(!crate::agent::installed_agents(None).is_empty(), reason("AGENT_CLI_MISSING", "Install Claude Code (`claude`) or Codex (`codex`) to let setup and sync launch an agent; otherwise prompts are printed.")));
    add("project_hub", available(true, None));
    add("mcp_server", available(true, None));

    let commands = command_descriptors();
    let mut by_kind = CommandsByKind::default();
    for c in &commands {
        match c.kind.as_str() {
            "preview" => by_kind.preview.push(c.clone()),
            "apply" => by_kind.apply.push(c.clone()),
            _ => by_kind.read.push(c.clone()),
        }
    }

    CapabilitiesReport {
        schema_version: 2,
        knobyte_version: crate::version::VERSION.to_string(),
        repository: RepositoryInfo {
            state: if scaffold { "ready".into() } else { "scaffold_missing".into() },
            initialization_state: initialization_state.to_string(),
            graph_index_state: graph_state,
            wiki_index_state: wiki_state.to_string(),
            root: config.project_root.to_string_lossy().to_string(),
            scaffold_root: config.scaffold_root.to_string_lossy().to_string(),
            scaffold_id: crate::config::persisted_scaffold_id(&config.scaffold_root),
            ai_tools: crate::config::load_ai_tools(&config.scaffold_root),
        },
        capabilities,
        commands,
        commands_by_kind: by_kind,
        exit_codes: exit_codes(),
        next_initialization_action,
        embedding,
    }
}
