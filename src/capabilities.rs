use serde::{Deserialize, Serialize};
use crate::config::KnobyteConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityItem {
    pub id: String,
    pub installed: bool,
    pub availability: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDescriptor {
    pub id: String,
    pub path: String,
    pub usage: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesReport {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub repository: RepositoryInfo,
    pub capabilities: Vec<CapabilityItem>,
    pub commands: Vec<CommandDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInfo {
    pub state: String,
    pub root: String,
    #[serde(rename = "scaffoldRoot")]
    pub scaffold_root: String,
    #[serde(rename = "scaffoldId")]
    pub scaffold_id: String,
}

pub fn get_capabilities(config: &KnobyteConfig) -> CapabilitiesReport {
    let repo_state = if config.scaffold_root.exists() {
        "ready".to_string()
    } else {
        "scaffold_missing".to_string()
    };

    let capabilities = vec![
        CapabilityItem { id: "code_graph".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "wiki".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "cozodb_graph".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "vector_search".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "datalog_engine".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "drift_check".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "team_identity".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "team_relay".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "team_inbox".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "team_workstreams".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "activity".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "project_hub".to_string(), installed: true, availability: "available".to_string() },
        CapabilityItem { id: "mcp_server".to_string(), installed: true, availability: "available".to_string() },
    ];

    let commands = vec![
        CommandDescriptor {
            id: "graph.status".to_string(),
            path: "knobyte graph status".to_string(),
            usage: "knobyte graph status --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "graph.query".to_string(),
            path: "knobyte graph query".to_string(),
            usage: "knobyte graph query <relation> <target> --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "graph.scope".to_string(),
            path: "knobyte graph scope".to_string(),
            usage: "knobyte graph scope <task...> --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "wiki.query".to_string(),
            path: "knobyte wiki query".to_string(),
            usage: "knobyte wiki query <text...> --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "wiki.show".to_string(),
            path: "knobyte wiki show".to_string(),
            usage: "knobyte wiki show <id> --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "check".to_string(),
            path: "knobyte check".to_string(),
            usage: "knobyte check --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "relay.list".to_string(),
            path: "knobyte relay list".to_string(),
            usage: "knobyte relay list --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "inbox.draft".to_string(),
            path: "knobyte inbox draft".to_string(),
            usage: "knobyte inbox draft list --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "timeline".to_string(),
            path: "knobyte timeline".to_string(),
            usage: "knobyte timeline --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "mcp".to_string(),
            path: "knobyte mcp".to_string(),
            usage: "knobyte mcp --sse --port 3001".to_string(),
            output: "sse".to_string(),
        },
        CommandDescriptor {
            id: "cozo.query".to_string(),
            path: "knobyte cozo query".to_string(),
            usage: "knobyte cozo query <script> --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "cozo.search".to_string(),
            path: "knobyte cozo search".to_string(),
            usage: "knobyte cozo search <query> [--target code|wiki] --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "cozo.pagerank".to_string(),
            path: "knobyte cozo pagerank".to_string(),
            usage: "knobyte cozo pagerank --json".to_string(),
            output: "json".to_string(),
        },
        CommandDescriptor {
            id: "cozo.shortest_path".to_string(),
            path: "knobyte cozo shortest-path".to_string(),
            usage: "knobyte cozo shortest-path <start> <target> --json".to_string(),
            output: "json".to_string(),
        },
    ];

    CapabilitiesReport {
        schema_version: 1,
        repository: RepositoryInfo {
            state: repo_state,
            root: config.project_root.to_string_lossy().to_string(),
            scaffold_root: config.scaffold_root.to_string_lossy().to_string(),
            scaffold_id: config.scaffold_id.clone(),
        },
        capabilities,
        commands,
    }
}
