use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use crate::config::KnobyteConfig;

pub mod anchor;
pub mod flow;
pub mod prompts;
pub mod templates;
pub mod update;

use templates::{render, templates_for_mode, TemplateVars, REQUIRED_POPULATED_FILES};

pub fn detect_tech_stack(project_root: &Path) -> Vec<String> {
    let mut stack = Vec::new();

    // 1. Rust and Cargo
    let cargo_toml = project_root.join("Cargo.toml");
    if cargo_toml.exists() {
        stack.push("**Rust**: High performance systems programming.".to_string());
        if let Ok(content) = fs::read_to_string(&cargo_toml) {
            let lower = content.to_lowercase();
            if lower.contains("axum") {
                stack.push("**Axum**: Async HTTP web application framework.".to_string());
            } else if lower.contains("actix-web") || lower.contains("actix_web") {
                stack.push("**Actix-web**: Async actor-based HTTP framework.".to_string());
            }
            if lower.contains("sqlx") {
                stack.push("**sqlx**: Async compile-time verified SQL database toolkit.".to_string());
            } else if lower.contains("diesel") {
                stack.push("**Diesel**: Safe, extensible ORM and Query builder.".to_string());
            }
            if lower.contains("postgres") || lower.contains("tokio-postgres") {
                stack.push("**PostgreSQL**: Relational database storage.".to_string());
            }
            if lower.contains("tigerbeetle") {
                stack.push("**TigerBeetle**: High-throughput distributed financial accounting database.".to_string());
            }
            if lower.contains("rusqlite") || lower.contains("sqlite") {
                stack.push("**SQLite**: Embedded relational database.".to_string());
            }
            if lower.contains("tokio") {
                stack.push("**Tokio**: Asynchronous runtime.".to_string());
            }
        }
    }

    // 2. Node / TypeScript / JavaScript
    let package_json = project_root.join("package.json");
    if package_json.exists() {
        if project_root.join("tsconfig.json").exists() {
            stack.push("**TypeScript**: Strongly typed JavaScript runtime.".to_string());
        } else {
            stack.push("**JavaScript**: Node.js ecosystem runtime.".to_string());
        }
        if let Ok(content) = fs::read_to_string(&package_json) {
            let lower = content.to_lowercase();
            if lower.contains("next") {
                stack.push("**Next.js**: React full-stack framework.".to_string());
            } else if lower.contains("react") {
                stack.push("**React**: Component-driven UI library.".to_string());
            }
            if lower.contains("express") {
                stack.push("**Express**: Web application framework.".to_string());
            }
        }
    }

    // 3. Python
    if project_root.join("pyproject.toml").exists() || project_root.join("requirements.txt").exists() {
        stack.push("**Python**: High-level dynamic language.".to_string());
    }

    // 4. SQL migrations
    let migrations_dir = project_root.join("migrations");
    if migrations_dir.exists() && migrations_dir.is_dir() {
        stack.push("**SQL Migrations**: Schema migrations and database contracts.".to_string());
    }

    stack
}

pub const SETUP_MODES: &[&str] = &["code-repo", "agent-memory", "monorepo", "docs-only"];

/// Patterns that must always be present in `.knobyte/.gitignore`.
const SCAFFOLD_GITIGNORE_REQUIRED: &[&str] = &["graph.db*", "wiki.db*", "cozo.db*", "local/"];

const SCAFFOLD_GITIGNORE_DEFAULT: &str = "# Knobyte local caches and derived databases\n*.db-wal\n*.db-shm\ngraph.db*\nwiki.db*\ncozo.db*\nlocal/\ncache/\n";

const ROOT_GITIGNORE_MARKER: &str = "# Knobyte local databases and cache";
const ROOT_GITIGNORE_ADDITION: &str = "\n# Knobyte local databases and cache\n.knobyte/graph.db*\n.knobyte/wiki.db*\n.knobyte/cozo.db*\n.knobyte/local/\n.knobyte/cache/\n";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetupAction {
    /// Absolute path of the file or directory.
    pub path: String,
    /// One of `create_dir`, `create_file`, `modify_file`.
    pub action: String,
    /// Short human-readable explanation.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupReport {
    pub dry_run: bool,
    pub mode: String,
    pub actions: Vec<SetupAction>,
}

enum PlannedWrite {
    Dir(PathBuf),
    File { path: PathBuf, content: String, modify: bool, detail: String },
}

fn plan_writes(config: &KnobyteConfig, mode: &str) -> Result<Vec<PlannedWrite>, String> {
    let mut plan = Vec::new();

    for dir in config.scaffold_dirs() {
        if !dir.is_dir() {
            plan.push(PlannedWrite::Dir(dir));
        }
    }

    // config.json: written when missing or when its content (e.g. mode) changes.
    let mut cfg = config.clone();
    cfg.mode = mode.to_string();
    let config_content = cfg.config_file_json().map_err(|e| e.to_string())?;
    let config_path = config.config_file_path();
    let existing_config = fs::read_to_string(&config_path).ok();
    match existing_config {
        None => plan.push(PlannedWrite::File {
            path: config_path,
            content: config_content,
            modify: false,
            detail: format!("project configuration (mode: {})", mode),
        }),
        Some(existing) if existing != config_content => plan.push(PlannedWrite::File {
            path: config_path,
            content: config_content,
            modify: true,
            detail: format!("update project configuration (mode: {})", mode),
        }),
        Some(_) => {}
    }

    // .knobyte/.gitignore
    let scaffold_gitignore = config.scaffold_root.join(".gitignore");
    match fs::read_to_string(&scaffold_gitignore) {
        Err(_) => plan.push(PlannedWrite::File {
            path: scaffold_gitignore,
            content: SCAFFOLD_GITIGNORE_DEFAULT.to_string(),
            modify: false,
            detail: "ignore derived databases and checkout-local state".to_string(),
        }),
        Ok(existing) => {
            let lines: Vec<&str> = existing.lines().map(|l| l.trim()).collect();
            let missing: Vec<&str> = SCAFFOLD_GITIGNORE_REQUIRED
                .iter()
                .copied()
                .filter(|p| !lines.contains(p))
                .collect();
            if !missing.is_empty() {
                let mut content = existing.clone();
                if !content.ends_with('\n') && !content.is_empty() {
                    content.push('\n');
                }
                for p in &missing {
                    content.push_str(p);
                    content.push('\n');
                }
                plan.push(PlannedWrite::File {
                    path: scaffold_gitignore,
                    content,
                    modify: true,
                    detail: format!("add ignore patterns: {}", missing.join(", ")),
                });
            }
        }
    }

    // Project root .gitignore (only when the project already has one).
    let root_gitignore = config.project_root.join(".gitignore");
    if let Ok(existing) = fs::read_to_string(&root_gitignore) {
        let already = existing.contains(ROOT_GITIGNORE_MARKER)
            || existing.contains(".knobyte/*.db")
            || existing.contains(".knobyte/graph.db");
        if !already {
            plan.push(PlannedWrite::File {
                path: root_gitignore,
                content: format!("{}{}", existing, ROOT_GITIGNORE_ADDITION),
                modify: true,
                detail: "append Knobyte local database and cache ignore rules".to_string(),
            });
        }
    }

    // Scaffold documents: created when missing, never overwritten.
    let vars = TemplateVars::new(config.project_name(), Vec::new());
    for (rel, template) in templates_for_mode(mode) {
        let path = config.scaffold_root.join(rel);
        if path.exists() {
            continue;
        }
        let content = if rel == "context/stack.md" {
            // Stack detection reads manifests, so only run it when the file is created.
            let vars = TemplateVars::new(config.project_name(), detect_tech_stack(&config.project_root));
            render(template, &vars)
        } else {
            render(template, &vars)
        };
        plan.push(PlannedWrite::File {
            path,
            content,
            modify: false,
            detail: describe_template(rel).to_string(),
        });
    }

    Ok(plan)
}

fn describe_template(rel: &str) -> &'static str {
    match rel {
        "AGENTS.md" => "always-loaded project anchor",
        "ROUTER.md" => "session bootstrap and routing table",
        "SETUP.md" => "manual population guide",
        "SYNC.md" => "drift repair guide",
        "HEARTBEAT.md" => "agent-memory heartbeat checks",
        "context/architecture.md" => "architecture overview (kb_architecture)",
        "context/stack.md" => "technology stack (kb_stack)",
        "context/conventions.md" => "coding conventions (kb_conventions)",
        "context/decisions.md" => "decision log (kb_decisions)",
        "context/setup.md" => "development setup (kb_setup)",
        "patterns/README.md" => "pattern format guide",
        "patterns/INDEX.md" => "pattern index",
        _ => "scaffold document",
    }
}

/// Project state that selects the population prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectState {
    /// No source files yet: populate from the user's intent.
    Fresh,
    /// Source files and an unpopulated scaffold: populate from code.
    Existing,
    /// Source files and an already populated scaffold: preserve it and finish setup.
    Partial,
}

impl ProjectState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProjectState::Fresh => "fresh",
            ProjectState::Existing => "existing",
            ProjectState::Partial => "partial",
        }
    }
}

const SOURCE_EXTENSIONS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "go", "rs", "java", "kt", "swift", "rb", "php", "c", "cpp", "cs",
    "ex", "exs", "zig", "lua", "dart", "scala", "clj", "erl", "hs", "ml", "vue", "svelte",
];

/// Whether every required scaffold file exists and none still carries the populate marker.
pub fn is_scaffold_populated(scaffold_root: &Path) -> bool {
    REQUIRED_POPULATED_FILES.iter().all(|rel| {
        fs::read_to_string(scaffold_root.join(rel))
            .map(|c| !templates::needs_population(&c))
            .unwrap_or(false)
    })
}

/// Required scaffold files that still need population (missing or carrying the marker).
pub fn unpopulated_files(scaffold_root: &Path) -> Vec<String> {
    REQUIRED_POPULATED_FILES
        .iter()
        .filter(|rel| {
            fs::read_to_string(scaffold_root.join(rel))
                .map(|c| templates::needs_population(&c))
                .unwrap_or(true)
        })
        .map(|s| s.to_string())
        .collect()
}

/// Whether the project has source files (searched to depth 4, skipping vendored trees).
pub fn has_source_files(project_root: &Path) -> bool {
    const SKIP: &[&str] = &["node_modules", ".knobyte", "vendor", ".git", "target", "dist", "build"];
    walkdir::WalkDir::new(project_root)
        .max_depth(4)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && SKIP.contains(&e.file_name().to_string_lossy().as_ref())))
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| SOURCE_EXTENSIONS.contains(&x))
                    .unwrap_or(false)
        })
}

pub fn detect_project_state(project_root: &Path, scaffold_root: &Path) -> ProjectState {
    let has_source = has_source_files(project_root);
    if has_source && is_scaffold_populated(scaffold_root) {
        ProjectState::Partial
    } else if has_source {
        ProjectState::Existing
    } else {
        ProjectState::Fresh
    }
}

/// Explicit mode, else the saved mode, else `code-repo`.
pub fn resolve_setup_mode(config: &KnobyteConfig, requested: Option<&str>) -> Result<String, String> {
    let mode = match requested {
        Some(m) => m.to_string(),
        None if config.config_file_path().exists() => config.mode.clone(),
        None => "code-repo".to_string(),
    };
    validate_setup_mode(&mode)?;
    Ok(mode)
}

fn to_action(w: &PlannedWrite) -> SetupAction {
    match w {
        PlannedWrite::Dir(p) => SetupAction {
            path: p.to_string_lossy().to_string(),
            action: "create_dir".to_string(),
            detail: "scaffold directory".to_string(),
        },
        PlannedWrite::File { path, modify, detail, .. } => SetupAction {
            path: path.to_string_lossy().to_string(),
            action: if *modify { "modify_file" } else { "create_file" }.to_string(),
            detail: detail.clone(),
        },
    }
}

/// Validate a setup mode.
pub fn validate_setup_mode(mode: &str) -> Result<(), String> {
    if SETUP_MODES.contains(&mode) {
        Ok(())
    } else {
        Err(format!(
            "Unknown setup mode '{}'. Valid modes: {}",
            mode,
            SETUP_MODES.join(", ")
        ))
    }
}

/// Compute (and unless `dry_run`, apply) the scaffold setup. Returns every file or
/// directory that was (or would be) created or modified. Existing content documents
/// are never overwritten.
pub fn apply_setup(config: &KnobyteConfig, mode: &str, dry_run: bool) -> Result<SetupReport, String> {
    validate_setup_mode(mode)?;
    let plan = plan_writes(config, mode)?;
    let actions: Vec<SetupAction> = plan.iter().map(to_action).collect();

    if !dry_run {
        for w in &plan {
            match w {
                PlannedWrite::Dir(p) => fs::create_dir_all(p).map_err(|e| format!("{}: {}", p.display(), e))?,
                PlannedWrite::File { path, content, .. } => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    fs::write(path, content).map_err(|e| format!("{}: {}", path.display(), e))?;
                }
            }
        }
    }

    Ok(SetupReport {
        dry_run,
        mode: mode.to_string(),
        actions,
    })
}

/// CLI-facing setup: applies (or previews) the setup and prints what changed.
pub fn run_setup(config: &KnobyteConfig, mode: &str, dry_run: bool) -> Result<(), String> {
    let report = apply_setup(config, mode, dry_run)?;
    let display = |p: &str| -> String {
        Path::new(p)
            .strip_prefix(&config.project_root)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| p.to_string())
    };
    if dry_run {
        println!(
            "Dry run: setup (mode: {}) in {} would make {} change(s):",
            report.mode,
            config.project_root.display(),
            report.actions.len()
        );
        for a in &report.actions {
            println!("  {:<12} {}  ({})", a.action, display(&a.path), a.detail);
        }
        if report.actions.is_empty() {
            println!("  (nothing to do; scaffold is up to date)");
        }
    } else {
        for a in report.actions.iter().filter(|a| a.action == "modify_file") {
            println!("Modified {}: {}", display(&a.path), a.detail);
        }
    }
    Ok(())
}
