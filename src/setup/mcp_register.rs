//! Registering Knobyte's MCP server (`knobyte mcp --stdio --profile core`) with each selected
//! AI tool.
//!
//! | Tool | File | Scope |
//! |------|------|-------|
//! | Claude Code | `.mcp.json` (`mcpServers.knobyte`) | project |
//! | Cursor | `.cursor/mcp.json` (`mcpServers.knobyte`) | project |
//! | VS Code / Copilot | `.vscode/mcp.json` (`servers.knobyte`, `type: stdio`) | project |
//! | OpenCode | `opencode.json` (`mcp.knobyte`, `type: local`) | project |
//! | Codex | `.codex/config.toml` (`[mcp_servers.knobyte]`, loaded in trusted projects) | project |
//! | Windsurf | `~/.codeium/windsurf/mcp_config.json` (`mcpServers.knobyte`) | user |
//!
//! Edits are non-destructive: the file is parsed, only the `knobyte` entry is added or
//! updated (every other server, key, comment and the formatting are kept), and a file that
//! cannot be parsed is left untouched and reported with the snippet to add by hand. Re-running
//! is a no-op. A user-level file is written only when the caller has explicit consent (an
//! interactive confirmation naming the file, or `--global-mcp`).

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::agent::find_on_path;
use crate::setup::jsonedit::{self, J};

/// Arguments of the registered server.
pub const MCP_ARGS: &[&str] = &["mcp", "--stdio", "--profile", "core"];

/// Variable Cursor and VS Code expand to the open workspace folder.
const WORKSPACE_FOLDER: &str = "${workspaceFolder}";

/// The program MCP clients should start: `knobyte` when it resolves on PATH to the running
/// binary, else the running binary's absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerCommand {
    pub command: String,
}

impl ServerCommand {
    pub fn resolve(path: Option<&OsStr>) -> Self {
        let exe = std::env::current_exe().ok();
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if let (Some(exe), Some(found)) = (&exe, find_on_path("knobyte", path)) {
            if canon(exe) == canon(&found) {
                return ServerCommand { command: "knobyte".into() };
            }
        }
        ServerCommand {
            command: exe.map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "knobyte".into()),
        }
    }

    /// The shell form, e.g. `knobyte mcp --stdio --profile core`.
    pub fn display(&self) -> String {
        let quote = |s: &str| if s.contains(' ') { format!("\"{}\"", s) } else { s.to_string() };
        std::iter::once(quote(&self.command)).chain(MCP_ARGS.iter().map(|a| a.to_string())).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpScope {
    /// A file in the repository (committed with the scaffold).
    Project,
    /// A file in the user's home directory.
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpOutcome {
    /// The file did not exist and was created.
    Created,
    /// The `knobyte` entry was added to an existing file.
    Added,
    /// An existing `knobyte` entry was brought up to date.
    Updated,
    /// The entry is already current.
    Unchanged,
    /// The file could not be parsed (or has an unexpected shape) and was left untouched.
    Unparseable,
    /// A user-level file that needs explicit consent; nothing was written.
    NeedsConsent,
    /// Writing failed.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResult {
    pub tool: String,
    /// Project-relative path, or `~/...` for a user-level file.
    pub path: String,
    pub scope: McpScope,
    pub outcome: McpOutcome,
    /// Nothing was written (dry run, re-run without `--tools`, or no consent).
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What to add by hand when the file was not written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

impl McpResult {
    /// Whether the file now (or after the planned write) registers Knobyte.
    pub fn registered(&self) -> bool {
        matches!(self.outcome, McpOutcome::Created | McpOutcome::Added | McpOutcome::Updated | McpOutcome::Unchanged)
    }
    /// Whether this result wrote (or would write) a file.
    pub fn changes(&self) -> bool {
        matches!(self.outcome, McpOutcome::Created | McpOutcome::Added | McpOutcome::Updated)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// JSON object at `path`.
    Json(&'static [&'static str]),
    /// `[mcp_servers.knobyte]` in TOML.
    CodexToml,
}

/// Where one tool's registration lives.
#[derive(Debug, Clone)]
pub struct McpTarget {
    pub tool: &'static str,
    pub file: PathBuf,
    pub display: String,
    pub scope: McpScope,
    format: Format,
}

/// The Windsurf user-level MCP file: `~/.codeium/windsurf/mcp_config.json`, or
/// `~/.config/devin/mcp_config.json` when only that configuration directory exists.
pub fn windsurf_config_path(home: &Path) -> (PathBuf, String) {
    let codeium = home.join(".codeium/windsurf");
    let devin = home.join(".config/devin");
    if !codeium.is_dir() && devin.is_dir() {
        (devin.join("mcp_config.json"), "~/.config/devin/mcp_config.json".into())
    } else {
        (codeium.join("mcp_config.json"), "~/.codeium/windsurf/mcp_config.json".into())
    }
}

/// Registration targets for `tools` (unknown tools are skipped; Windsurf needs `home`).
pub fn mcp_targets(project_root: &Path, home: Option<&Path>, tools: &[String]) -> Vec<McpTarget> {
    let mut out = Vec::new();
    let project = |tool, rel: &str, format| McpTarget {
        tool,
        file: project_root.join(rel),
        display: rel.to_string(),
        scope: McpScope::Project,
        format,
    };
    for tool in tools {
        match tool.as_str() {
            "claude" => out.push(project("claude", ".mcp.json", Format::Json(&["mcpServers", "knobyte"]))),
            "cursor" => out.push(project("cursor", ".cursor/mcp.json", Format::Json(&["mcpServers", "knobyte"]))),
            "copilot" => out.push(project("copilot", ".vscode/mcp.json", Format::Json(&["servers", "knobyte"]))),
            "opencode" => out.push(project("opencode", "opencode.json", Format::Json(&["mcp", "knobyte"]))),
            "codex" => out.push(project("codex", ".codex/config.toml", Format::CodexToml)),
            "windsurf" => {
                if let Some(home) = home {
                    let (file, display) = windsurf_config_path(home);
                    out.push(McpTarget {
                        tool: "windsurf",
                        file,
                        display,
                        scope: McpScope::User,
                        format: Format::Json(&["mcpServers", "knobyte"]),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Project-scoped files registration may write, for the commit checkpoint.
pub fn project_mcp_paths(tools: &[String]) -> Vec<String> {
    mcp_targets(Path::new(""), None, tools)
        .into_iter()
        .filter(|t| t.scope == McpScope::Project)
        .map(|t| t.display)
        .collect()
}

fn args_with(extra: &[&str]) -> Vec<String> {
    MCP_ARGS.iter().chain(extra.iter()).map(|s| s.to_string()).collect()
}

/// The desired `knobyte` entry for `target`.
fn desired_entry(target: &McpTarget, cmd: &ServerCommand, project_root: &Path) -> J {
    let command = J::Str(cmd.command.clone());
    match target.tool {
        "cursor" => J::obj(vec![("command", command), ("args", J::strs(&args_with(&["--root", WORKSPACE_FOLDER])))]),
        "copilot" => J::obj(vec![
            ("type", J::Str("stdio".into())),
            ("command", command),
            ("args", J::strs(&args_with(&["--root", WORKSPACE_FOLDER]))),
        ]),
        "opencode" => {
            let mut full = vec![cmd.command.clone()];
            full.extend(args_with(&[]));
            J::obj(vec![("type", J::Str("local".into())), ("command", J::strs(&full)), ("enabled", J::Bool(true))])
        }
        "windsurf" => {
            let root = project_root.to_string_lossy().to_string();
            J::obj(vec![("command", command), ("args", J::strs(&args_with(&["--root", &root])))])
        }
        _ => J::obj(vec![("command", command), ("args", J::strs(&args_with(&[])))]),
    }
}

fn toml_quote(s: &str) -> String {
    toml_edit::Value::from(s).to_string().trim().to_string()
}

/// What to add by hand.
pub fn snippet(target: &McpTarget, cmd: &ServerCommand, project_root: &Path) -> String {
    let entry = desired_entry(target, cmd, project_root);
    match target.format {
        Format::Json(path) => {
            let mut doc = entry;
            for key in path.iter().rev() {
                doc = J::Obj(vec![(key.to_string(), doc)]);
            }
            doc.render("  ", "", "\n")
        }
        Format::CodexToml => {
            let args: Vec<String> = MCP_ARGS.iter().map(|a| toml_quote(a)).collect();
            format!("[mcp_servers.knobyte]\ncommand = {}\nargs = [{}]", toml_quote(&cmd.command), args.join(", "))
        }
    }
}

/// Plan the new file content: `(outcome, new content, detail)`.
fn plan(target: &McpTarget, current: Option<&str>, cmd: &ServerCommand, project_root: &Path) -> (McpOutcome, Option<String>, Option<String>) {
    let entry = desired_entry(target, cmd, project_root);
    match target.format {
        Format::Json(path) => {
            let Some(text) = current else {
                let mut doc = entry;
                for key in path.iter().rev() {
                    doc = J::Obj(vec![(key.to_string(), doc)]);
                }
                return (McpOutcome::Created, Some(doc.document()), None);
            };
            plan_json(text, path, &entry)
        }
        Format::CodexToml => plan_toml(current.unwrap_or(""), cmd, current.is_none()),
    }
}

fn plan_json(text: &str, path: &[&str], entry: &J) -> (McpOutcome, Option<String>, Option<String>) {
    let unparseable = |e: String| (McpOutcome::Unparseable, None, Some(e));
    let existing = match jsonedit::get(text, path) {
        Ok(v) => v,
        Err(e) => return unparseable(e),
    };
    let J::Obj(fields) = entry else { return unparseable("internal: entry is not an object".into()) };
    match existing {
        Some(serde_json::Value::Object(map)) => {
            let mut out = text.to_string();
            let mut changed = false;
            for (k, v) in fields {
                if map.get(k) != Some(&v.to_value()) {
                    let mut p: Vec<&str> = path.to_vec();
                    p.push(k);
                    match jsonedit::upsert(&out, &p, v) {
                        Ok(t) => out = t,
                        Err(e) => return unparseable(e),
                    }
                    changed = true;
                }
            }
            if changed {
                (McpOutcome::Updated, Some(out), None)
            } else {
                (McpOutcome::Unchanged, None, None)
            }
        }
        Some(_) => match jsonedit::upsert(text, path, entry) {
            Ok(t) => (McpOutcome::Updated, Some(t), None),
            Err(e) => unparseable(e),
        },
        None => match jsonedit::upsert(text, path, entry) {
            Ok(t) => (McpOutcome::Added, Some(t), None),
            Err(e) => unparseable(e),
        },
    }
}

fn plan_toml(text: &str, cmd: &ServerCommand, missing: bool) -> (McpOutcome, Option<String>, Option<String>) {
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    let mut doc: DocumentMut = match text.parse() {
        Ok(d) => d,
        Err(e) => return (McpOutcome::Unparseable, None, Some(e.to_string().trim().to_string())),
    };
    let servers = doc.entry("mcp_servers").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let Some(servers) = servers.as_table_like_mut() else {
        return (McpOutcome::Unparseable, None, Some("`mcp_servers` is not a table".into()));
    };
    let existed = servers.contains_key("knobyte");
    let entry = servers.entry("knobyte").or_insert(Item::Table(Table::new()));
    let Some(entry) = entry.as_table_like_mut() else {
        return (McpOutcome::Unparseable, None, Some("`mcp_servers.knobyte` is not a table".into()));
    };
    let args: Vec<&str> = MCP_ARGS.to_vec();
    let current_cmd = entry.get("command").and_then(|v| v.as_str()).map(str::to_string);
    let current_args: Option<Vec<String>> = entry
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
    let want_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    if existed && current_cmd.as_deref() == Some(cmd.command.as_str()) && current_args.as_ref() == Some(&want_args) {
        return (McpOutcome::Unchanged, None, None);
    }
    if current_cmd.as_deref() != Some(cmd.command.as_str()) {
        entry.insert("command", value(cmd.command.as_str()));
    }
    if current_args.as_ref() != Some(&want_args) {
        entry.insert("args", value(args.iter().copied().collect::<Array>()));
    }
    let outcome = match (missing, existed) {
        (true, _) => McpOutcome::Created,
        (false, false) => McpOutcome::Added,
        (false, true) => McpOutcome::Updated,
    };
    (outcome, Some(doc.to_string()), None)
}

/// Options for [`register_mcp`].
#[derive(Debug, Clone, Default)]
pub struct McpOptions {
    /// Report only; write nothing.
    pub dry_run: bool,
    /// Explicit consent to write user-level files.
    pub allow_user_files: bool,
}

/// Register Knobyte with every target (or plan it, with `dry_run`).
pub fn register_mcp(project_root: &Path, targets: &[McpTarget], cmd: &ServerCommand, opts: &McpOptions) -> Vec<McpResult> {
    targets
        .iter()
        .map(|t| {
            let current = fs::read_to_string(&t.file).ok();
            let exists = t.file.exists();
            let (mut outcome, content, mut detail) = if exists && current.is_none() {
                (McpOutcome::Unparseable, None, Some("the file is not readable UTF-8 text".into()))
            } else {
                plan(t, current.as_deref(), cmd, project_root)
            };
            let needs_consent = t.scope == McpScope::User && !opts.allow_user_files && content.is_some();
            let mut dry_run = opts.dry_run;
            if needs_consent {
                outcome = McpOutcome::NeedsConsent;
                dry_run = true;
            } else if let (Some(content), false) = (&content, opts.dry_run) {
                let write = t.file.parent().map(fs::create_dir_all).unwrap_or(Ok(())).and_then(|_| fs::write(&t.file, content));
                if let Err(e) = write {
                    outcome = McpOutcome::Failed;
                    detail = Some(e.to_string());
                }
            }
            if t.tool == "codex" && outcome != McpOutcome::Unparseable && detail.is_none() {
                detail = Some("Codex loads project config only once you trust this project".into());
            }
            let snippet = matches!(outcome, McpOutcome::Unparseable | McpOutcome::NeedsConsent | McpOutcome::Failed)
                .then(|| snippet(t, cmd, project_root));
            McpResult {
                tool: t.tool.to_string(),
                path: t.display.clone(),
                scope: t.scope,
                outcome,
                dry_run,
                detail,
                snippet,
            }
        })
        .collect()
}

/// Human line for one result.
pub fn describe(r: &McpResult) -> String {
    let name = crate::setup::anchor::tool_display_name(&r.tool);
    let would = r.dry_run && r.changes();
    let line = match r.outcome {
        McpOutcome::Created if would => format!("Would create {} (Knobyte MCP server for {})", r.path, name),
        McpOutcome::Created => format!("Created {} (Knobyte MCP server for {})", r.path, name),
        McpOutcome::Added if would => format!("Would add the Knobyte MCP server to {} ({})", r.path, name),
        McpOutcome::Added => format!("Added the Knobyte MCP server to {} ({})", r.path, name),
        McpOutcome::Updated if would => format!("Would update the Knobyte MCP server in {} ({})", r.path, name),
        McpOutcome::Updated => format!("Updated the Knobyte MCP server in {} ({})", r.path, name),
        McpOutcome::Unchanged => format!("{} already registers the Knobyte MCP server ({})", r.path, name),
        McpOutcome::Unparseable => format!(
            "{} was left untouched because it could not be parsed{}. Add the Knobyte MCP server by hand:",
            r.path,
            r.detail.as_deref().map(|d| format!(" ({})", d)).unwrap_or_default()
        ),
        McpOutcome::NeedsConsent => format!(
            "{} reads MCP servers only from the user-level {}; not changed (rerun with --global-mcp, or add this entry yourself):",
            name, r.path
        ),
        McpOutcome::Failed => format!(
            "{} could not be written ({}). Add the Knobyte MCP server by hand:",
            r.path,
            r.detail.as_deref().unwrap_or("unknown error")
        ),
    };
    match (&r.detail, r.outcome) {
        (Some(d), McpOutcome::Created | McpOutcome::Added | McpOutcome::Updated | McpOutcome::Unchanged) => {
            format!("{}; {}", line, d)
        }
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> ServerCommand {
        ServerCommand { command: "knobyte".into() }
    }

    fn run(root: &Path, home: &Path, tool: &str, allow: bool) -> McpResult {
        let targets = mcp_targets(root, Some(home), &[tool.to_string()]);
        register_mcp(root, &targets, &cmd(), &McpOptions { dry_run: false, allow_user_files: allow }).remove(0)
    }

    #[test]
    fn every_client_creates_merges_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        let home = d.path().join("h");
        fs::create_dir_all(&root).unwrap();
        let cases: &[(&str, &str, &str, &str)] = &[
            ("claude", ".mcp.json", "{\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\"}\n  }\n}\n", "\"other\": {\"command\": \"x\"}"),
            ("cursor", ".cursor/mcp.json", "{\"mcpServers\": {\"other\": {\"command\": \"x\"}}}", "\"other\": {\"command\": \"x\"}"),
            (
                "copilot",
                ".vscode/mcp.json",
                "{\n  // mine\n  \"inputs\": [],\n  \"servers\": {\"other\": {\"type\": \"stdio\", \"command\": \"x\"}}\n}\n",
                "// mine",
            ),
            ("opencode", "opencode.json", "{\n  \"model\": \"m\",\n  \"mcp\": {\"other\": {\"type\": \"remote\"}}\n}\n", "\"model\": \"m\""),
            ("codex", ".codex/config.toml", "model = \"o3\" # keep\n\n[mcp_servers.other]\ncommand = \"x\"\n", "model = \"o3\" # keep"),
        ];
        for (tool, rel, existing, kept) in cases {
            let file = root.join(rel);
            // Create.
            let _ = fs::remove_file(&file);
            let r = run(&root, &home, tool, false);
            assert_eq!(r.outcome, McpOutcome::Created, "{}", tool);
            let created = fs::read_to_string(&file).unwrap();
            assert!(created.contains("\"--profile\"") || created.contains("args = [\"mcp\""), "{}: {}", tool, created);
            assert_eq!(run(&root, &home, tool, false).outcome, McpOutcome::Unchanged, "{}", tool);
            // Merge, preserving other servers and formatting.
            fs::write(&file, existing).unwrap();
            let r = run(&root, &home, tool, false);
            assert_eq!(r.outcome, McpOutcome::Added, "{}", tool);
            let merged = fs::read_to_string(&file).unwrap();
            assert!(merged.contains(kept), "{}: {}", tool, merged);
            assert!(merged.contains("other"), "{}: {}", tool, merged);
            assert!(merged.contains("knobyte"), "{}: {}", tool, merged);
            assert_eq!(run(&root, &home, tool, false).outcome, McpOutcome::Unchanged, "{}", tool);
            assert_eq!(fs::read_to_string(&file).unwrap(), merged, "{}: idempotent", tool);
            // A stale entry is updated in place.
            let stale = merged.replace("\"core\"", "\"full\"");
            fs::write(&file, &stale).unwrap();
            assert_eq!(run(&root, &home, tool, false).outcome, McpOutcome::Updated, "{}", tool);
            assert_eq!(fs::read_to_string(&file).unwrap(), merged, "{}: update restores", tool);
            // Unparseable files are never overwritten.
            let garbage = if tool == &"codex" { "[mcp_servers\nbroken" } else { "{\"mcpServers\": " };
            fs::write(&file, garbage).unwrap();
            let r = run(&root, &home, tool, false);
            assert_eq!(r.outcome, McpOutcome::Unparseable, "{}", tool);
            assert!(r.snippet.as_deref().unwrap().contains("knobyte"));
            assert_eq!(fs::read_to_string(&file).unwrap(), garbage);
        }
        // Shapes per client.
        let _ = fs::remove_file(root.join(".vscode/mcp.json"));
        run(&root, &home, "copilot", false);
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join(".vscode/mcp.json")).unwrap()).unwrap();
        assert_eq!(v["servers"]["knobyte"]["type"], "stdio");
        assert_eq!(v["servers"]["knobyte"]["args"][5], "${workspaceFolder}");
        let _ = fs::remove_file(root.join("opencode.json"));
        run(&root, &home, "opencode", false);
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join("opencode.json")).unwrap()).unwrap();
        assert_eq!(v["mcp"]["knobyte"], serde_json::json!({"type": "local", "command": ["knobyte", "mcp", "--stdio", "--profile", "core"], "enabled": true}));
        let _ = fs::remove_file(root.join(".mcp.json"));
        run(&root, &home, "claude", false);
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["knobyte"], serde_json::json!({"command": "knobyte", "args": ["mcp", "--stdio", "--profile", "core"]}));
    }

    #[test]
    fn user_level_file_needs_consent() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("p");
        let home = d.path().join("h");
        fs::create_dir_all(home.join(".codeium/windsurf")).unwrap();
        let file = home.join(".codeium/windsurf/mcp_config.json");
        fs::write(&file, "{\"mcpServers\": {\"other\": {\"command\": \"x\"}}}").unwrap();
        let r = run(&root, &home, "windsurf", false);
        assert_eq!(r.outcome, McpOutcome::NeedsConsent);
        assert_eq!(r.path, "~/.codeium/windsurf/mcp_config.json");
        assert!(r.snippet.as_deref().unwrap().contains("--root"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"mcpServers\": {\"other\": {\"command\": \"x\"}}}");
        assert!(describe(&r).contains("~/.codeium/windsurf/mcp_config.json"));
        let r = run(&root, &home, "windsurf", true);
        assert_eq!(r.outcome, McpOutcome::Added);
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["knobyte"]["args"][5], root.to_string_lossy().as_ref());
        // Already current: no consent needed to say so.
        assert_eq!(run(&root, &home, "windsurf", false).outcome, McpOutcome::Unchanged);
        // Dry runs never write.
        let other = tempfile::tempdir().unwrap();
        let targets = mcp_targets(other.path(), None, &["claude".to_string()]);
        let r = register_mcp(other.path(), &targets, &cmd(), &McpOptions { dry_run: true, allow_user_files: true });
        assert_eq!(r[0].outcome, McpOutcome::Created);
        assert!(r[0].dry_run && !other.path().join(".mcp.json").exists());
    }
}
