//! Scaffold templates shipped inside the binary (`templates/` at the repository root).
//!
//! Every populate-able template carries [`POPULATE_MARKER`]; the populating agent removes it
//! once the file holds real content, and setup treats the scaffold as populated when no
//! required file carries it.

/// Marker comment present in unpopulated scaffold files.
pub const POPULATE_MARKER: &str = "<!-- knobyte:populate -->";

/// (scaffold-relative path, template) for `code-repo` (and `monorepo` / `docs-only`) mode.
pub const SCAFFOLD_TEMPLATES: &[(&str, &str)] = &[
    ("AGENTS.md", include_str!("../../templates/scaffold/AGENTS.md")),
    ("ROUTER.md", include_str!("../../templates/scaffold/ROUTER.md")),
    ("SETUP.md", include_str!("../../templates/scaffold/SETUP.md")),
    ("SYNC.md", include_str!("../../templates/scaffold/SYNC.md")),
    ("context/architecture.md", include_str!("../../templates/scaffold/context/architecture.md")),
    ("context/stack.md", include_str!("../../templates/scaffold/context/stack.md")),
    ("context/conventions.md", include_str!("../../templates/scaffold/context/conventions.md")),
    ("context/decisions.md", include_str!("../../templates/scaffold/context/decisions.md")),
    ("context/setup.md", include_str!("../../templates/scaffold/context/setup.md")),
    ("patterns/README.md", include_str!("../../templates/scaffold/patterns/README.md")),
    ("patterns/INDEX.md", include_str!("../../templates/scaffold/patterns/INDEX.md")),
];

/// `agent-memory` overrides and additions.
pub const AGENT_MEMORY_TEMPLATES: &[(&str, &str)] = &[
    ("AGENTS.md", include_str!("../../templates/agent-memory/AGENTS.md")),
    ("ROUTER.md", include_str!("../../templates/agent-memory/ROUTER.md")),
    ("HEARTBEAT.md", include_str!("../../templates/agent-memory/HEARTBEAT.md")),
];

/// Knobyte-owned infrastructure files that `knobyte update` may refresh in place: they hold
/// no project content.
pub const INFRASTRUCTURE_FILES: &[&str] = &["SETUP.md", "SYNC.md", "patterns/README.md"];

/// Files that must be populated before setup can finalize.
pub const REQUIRED_POPULATED_FILES: &[&str] = &[
    "AGENTS.md",
    "ROUTER.md",
    "context/architecture.md",
    "context/stack.md",
    "context/conventions.md",
    "context/decisions.md",
    "context/setup.md",
];

/// Full rules template for Cursor, Windsurf and Copilot (identical copies).
pub const TOOL_RULES_TEMPLATE: &str = include_str!("../../templates/tool-config/rules.md");
/// OpenCode configuration template.
pub const OPENCODE_TEMPLATE: &str = include_str!("../../templates/tool-config/opencode.json");

/// The template list for `mode` (agent-memory files override the shared ones).
pub fn templates_for_mode(mode: &str) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&str, &str)> = SCAFFOLD_TEMPLATES.to_vec();
    if mode == "agent-memory" {
        for (path, content) in AGENT_MEMORY_TEMPLATES {
            match out.iter_mut().find(|(p, _)| p == path) {
                Some(slot) => slot.1 = content,
                None => out.push((path, content)),
            }
        }
    }
    out
}

/// Values substituted into templates.
pub struct TemplateVars {
    pub project_name: String,
    pub today: String,
    /// Markdown bullet list of detected technologies (may be empty).
    pub detected_stack: Vec<String>,
}

impl TemplateVars {
    pub fn new(project_name: String, detected_stack: Vec<String>) -> Self {
        // The name lands in unquoted YAML scalars; keep it free of YAML indicators.
        let project_name: String = project_name
            .chars()
            .map(|c| if matches!(c, ':' | '#' | '"' | '\'' | '{' | '}' | '[' | ']' | '|' | '>' | '&' | '*' | '!' | '%' | '@' | '`') { '-' } else { c })
            .collect();
        let project_name = if project_name.trim().is_empty() { "Project".to_string() } else { project_name.trim().to_string() };
        Self {
            project_name,
            today: chrono::Local::now().format("%Y-%m-%d").to_string(),
            detected_stack,
        }
    }
}

/// Render `template` with `vars`.
pub fn render(template: &str, vars: &TemplateVars) -> String {
    let stack = if vars.detected_stack.is_empty() {
        String::new()
    } else {
        let mut s = String::from("Detected from the repository manifests:\n");
        for item in &vars.detected_stack {
            s.push_str(&format!("- {}\n", item));
        }
        s.push('\n');
        s
    };
    template
        .replace("{{PROJECT_NAME}}", &vars.project_name)
        .replace("{{TODAY}}", &vars.today)
        .replace("{{DETECTED_STACK}}\n", &stack)
        .replace("{{DETECTED_STACK}}", &stack)
}

/// Whether one file's content still carries the populate marker.
pub fn needs_population(content: &str) -> bool {
    content.contains(POPULATE_MARKER)
}
