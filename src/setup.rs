use std::fs;
use std::path::Path;
use crate::config::KnobyteConfig;

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

pub fn run_setup(config: &KnobyteConfig, mode: &str, dry_run: bool) -> Result<(), String> {
    if dry_run {
        println!("Dry run: would create scaffold at {}", config.scaffold_root.display());
        return Ok(());
    }

    config.ensure_scaffold_dirs().map_err(|e| e.to_string())?;
    config.save().map_err(|e| e.to_string())?;

    // Create .knobyte/.gitignore
    let gitignore_path = config.scaffold_root.join(".gitignore");
    if !gitignore_path.exists() {
        let gitignore_content = "# Knobyte local caches and derived databases\n*.db*\n*.db-wal\n*.db-shm\ncozo.db/\ngraph.db*\nwiki.db*\nlocal/\ncache/\n";
        fs::write(gitignore_path, gitignore_content).map_err(|e| e.to_string())?;
    }

    // Update project root .gitignore if present to prevent accidental check-in of local DBs
    let root_gitignore = config.project_root.join(".gitignore");
    if root_gitignore.exists() {
        if let Ok(existing) = fs::read_to_string(&root_gitignore) {
            if !existing.contains(".knobyte/*.db") && !existing.contains(".knobyte/graph.db") {
                let addition = "\n# Knobyte local databases and cache\n.knobyte/*.db*\n.knobyte/local/\n.knobyte/cache/\n.knobyte/cozo.db/\n";
                let _ = fs::write(&root_gitignore, format!("{}{}", existing, addition));
            }
        }
    }

    // Create AGENTS.md
    let agents_md_path = config.scaffold_root.join("AGENTS.md");
    if !agents_md_path.exists() {
        let agents_md_content = r#"# Knobyte Project Memory Policy for AI Agents

Welcome to this repository. This project uses **Knobyte** to maintain persistent team memory, architecture groundings, and handoffs.

## Policy & Conventions
1. **Always read context first**: Before modifying complex subsystems, consult `.knobyte/ROUTER.md` and `.knobyte/context/` to understand existing architectural constraints and decisions.
2. **Query the Code Graph**: Use `knobyte graph query where-defined <name>` or `knobyte graph scope <task>` to retrieve deterministic code evidence.
3. **Record Discoveries**: When learning an important edge case, decision, or discovery, run `knobyte log <message> --kind decision` or prepare an Inbox draft (`knobyte inbox draft save`).
4. **Prepare Handoffs**: When ending a session or passing work to a teammate, draft a Relay with `knobyte relay draft save`.
"#;
        fs::write(agents_md_path, agents_md_content).map_err(|e| e.to_string())?;
    }

    // Create ROUTER.md
    let router_md_path = config.scaffold_root.join("ROUTER.md");
    if !router_md_path.exists() {
        let router_md_content = r#"# Knobyte Knowledge Router

This router maps development tasks to relevant project context.

| Task Kind | Relevant Context | Commands to Run |
|---|---|---|
| Architecture & Stack | `.knobyte/context/stack.md`, `.knobyte/context/architecture.md` | `knobyte wiki show kb_stack` |
| Coding Conventions | `.knobyte/context/conventions.md` | `knobyte wiki show kb_conventions` |
| Code Navigation | Code Graph SQLite Index | `knobyte graph scope "<task>"` |
| Handoff / Continuity | `.knobyte/relays/` | `knobyte relay list` |
| Knowledge Proposals | `.knobyte/inbox/` | `knobyte inbox draft list` |
"#;
        fs::write(router_md_path, router_md_content).map_err(|e| e.to_string())?;
    }

    // Create project-aware context/stack.md
    let stack_path = config.context_dir().join("stack.md");
    if !stack_path.exists() {
        let detected = detect_tech_stack(&config.project_root);
        let project_name = config.project_name();

        let (summary, status, body) = if detected.is_empty() {
            (
                format!("Technology stack specification for {}.", project_name),
                "draft",
                "<!-- [NEEDS_SPECIFICATION] Please define the technologies, languages, and frameworks used by this project. -->\n".to_string(),
            )
        } else {
            let items = detected.iter().map(|item| format!("- {}", item)).collect::<Vec<_>>().join("\n");
            (
                format!("Detected programming languages, frameworks, and key libraries for {}.", project_name),
                "active",
                format!("This project is built using:\n{}\n", items),
            )
        };

        let stack_content = format!(
            r#"---
id: kb_stack
title: Technology Stack
type: architecture
summary: {}
status: {}
revision: 1
---

# Technology Stack

{}
"#,
            summary, status, body
        );
        fs::write(stack_path, stack_content).map_err(|e| e.to_string())?;
    }

    // Agent memory mode additions
    if mode == "agent-memory" {
        let heartbeat_path = config.scaffold_root.join("HEARTBEAT.md");
        if !heartbeat_path.exists() {
            let heartbeat_content = r#"# Persistent Agent Memory & Health Contract

This repository operates in `agent-memory` mode. Periodic heartbeat runs verify scaffold consistency, prune stale temporary states, and check memory health.
"#;
            fs::write(heartbeat_path, heartbeat_content).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}
