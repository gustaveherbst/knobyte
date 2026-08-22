//! Setup / agent integration parity: scanner fixtures, managed blocks and anchors, skill
//! no-clobber, the agent launcher (against fake `claude` / `codex` scripts, never the real
//! CLIs), completion, and a fresh setup scoring 100 on `knobyte check`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use knobyte::agent::{run_agent, AgentEvent, AgentTool, LaunchFailure, LaunchOptions};
use knobyte::config::KnobyteConfig;
use knobyte::scanner;
use knobyte::skills::{sync_skills, sync_skills_with, SkillSyncOptions};
use tempfile::tempdir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_knobyte"))
}

/// A home directory with no AI tool configuration (tool detection never sees the real one).
fn empty_home() -> PathBuf {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| tempdir().unwrap()).path().to_path_buf()
}

/// System directories only, so the developer's real agent CLIs are never found.
fn system_path(extra: Option<&Path>) -> OsString {
    let mut paths: Vec<PathBuf> = extra.map(|p| vec![p.to_path_buf()]).unwrap_or_default();
    paths.extend(["/usr/bin", "/bin", "/usr/local/bin"].map(PathBuf::from).into_iter().filter(|p| !p.join("claude").exists() && !p.join("codex").exists()));
    std::env::join_paths(paths).unwrap()
}

/// Run the knobyte binary in `dir` with agent launching hard-disabled unless `extra_path`
/// supplies fake agent CLIs (prepended to PATH). HOME, PATH and the app directory are
/// isolated so tool detection only sees what a test creates.
fn knobyte(dir: &Path, args: &[&str], extra_path: Option<&Path>) -> std::process::Output {
    knobyte_with(dir, args, extra_path, &empty_home())
}

fn knobyte_with(dir: &Path, args: &[&str], extra_path: Option<&Path>, home: &Path) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env_remove("CI")
        .env("HOME", home)
        .env("KNOBYTE_APPLICATIONS_DIR", "")
        .env("PATH", system_path(extra_path));
    match extra_path {
        Some(_) => {
            cmd.env_remove("KNOBYTE_NO_AGENT_LAUNCH");
        }
        None => {
            cmd.env("KNOBYTE_NO_AGENT_LAUNCH", "1");
        }
    }
    cmd.output().expect("run knobyte")
}

fn git_init(dir: &Path) {
    let run = |args: &[&str]| {
        Command::new("git").args(args).current_dir(dir).output().unwrap();
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "dev@example.com"]);
    run(&["config", "user.name", "Dev"]);
}

#[cfg(unix)]
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

// ------------------------------------------------------------------------------ scanner

#[test]
fn scanner_reads_node_fixture() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(
        root.join("package.json"),
        r#"{"name":"shop","version":"1.2.0","dependencies":{"express":"^4"},"devDependencies":{"vitest":"^1"},"scripts":{"test":"vitest"}}"#,
    )
    .unwrap();
    fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
    fs::write(root.join("vitest.config.ts"), "").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    fs::write(root.join("eslint.config.js"), "").unwrap();
    fs::write(root.join(".prettierrc"), "{}").unwrap();
    fs::create_dir_all(root.join("src/routes")).unwrap();
    fs::write(root.join("src/index.ts"), "export {}").unwrap();
    fs::write(root.join("src/routes/a.test.ts"), "").unwrap();
    fs::create_dir_all(root.join("node_modules/x")).unwrap();
    fs::write(root.join("node_modules/x/index.js"), "").unwrap();
    fs::write(root.join("README.md"), "x".repeat(5000)).unwrap();

    let brief = scanner::scan(root);
    let m = brief.manifest.unwrap();
    assert_eq!(m.kind, "package.json");
    assert_eq!(m.name.as_deref(), Some("shop"));
    assert_eq!(m.scripts.get("test").map(String::as_str), Some("vitest"));
    assert!(brief.entry_points.iter().any(|e| e.path == "src/index.ts" && e.kind == "main"));
    assert!(brief.entry_points.iter().any(|e| e.path == "src/routes/a.test.ts" && e.kind == "test"));
    assert!(brief.entry_points.iter().any(|e| e.path == "tsconfig.json" && e.kind == "config"));
    assert!(!brief.entry_points.iter().any(|e| e.path.contains("node_modules")));
    assert_eq!(brief.tooling.test_runner.as_deref(), Some("vitest"));
    assert_eq!(brief.tooling.linter.as_deref(), Some("eslint"));
    assert_eq!(brief.tooling.formatter.as_deref(), Some("prettier"));
    assert_eq!(brief.tooling.package_manager.as_deref(), Some("pnpm"));
    assert!(brief.folder_tree.iter().any(|f| f.name == "src"));
    assert!(!brief.folder_tree.iter().any(|f| f.name == "node_modules"));
    assert!(brief.readme.unwrap().ends_with("... (truncated)"));

    // `knobyte init --json` emits the same brief.
    let out = knobyte(root, &["init", "--json"], None);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["manifest"]["type"], "package.json");
    assert_eq!(v["tooling"]["testRunner"], "vitest");
}

#[test]
fn scanner_reads_rust_fixture() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"svc\"\nversion = \"0.3.0\"\n\n[dependencies]\naxum = \"0.8\"\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(root.join("tests/api.rs"), "").unwrap();
    let brief = scanner::scan(root);
    assert_eq!(brief.manifest.as_ref().unwrap().dependencies.get("axum").map(String::as_str), Some("0.8"));
    assert!(brief.entry_points.iter().any(|e| e.path == "src/main.rs"));
    assert!(brief.entry_points.iter().any(|e| e.path == "tests/api.rs" && e.kind == "test"));
    assert_eq!(brief.tooling.build_tool.as_deref(), Some("cargo"));
    assert_eq!(brief.tooling.test_runner.as_deref(), Some("cargo test"));
    let prompt = scanner::build_prompt(&brief);
    assert!(prompt.contains("<brief>") && prompt.contains(".knobyte/context/stack.md"));
}

#[test]
fn scanner_reads_swiftpm_fixture() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::write(
        root.join("Package.swift"),
        r#"// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "AgentTaskMonitor",
    platforms: [.macOS(.v14)],
    products: [
        // .executable(name: "commented", targets: ["commented"]),
        .library(name: "Core", targets: ["Core"]),
        .executable(name: "agenttask", targets: ["agenttask"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-argument-parser.git", from: "1.3.0"),
    ],
    targets: [
        .target(name: "Core", dependencies: []),
        .executableTarget(
            name: "agenttask",
            dependencies: ["Core", .product(name: "ArgumentParser", package: "swift-argument-parser")],
            path: "Sources/agenttask"
        ),
        .executableTarget(name: "MenuApp", dependencies: [.target(name: "Core")]),
        .testTarget(name: "CoreTests", dependencies: ["Core"]),
    ]
)
"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("Sources/Core")).unwrap();
    fs::write(root.join("Sources/Core/Model.swift"), "struct Model {}").unwrap();
    fs::create_dir_all(root.join("Sources/agenttask")).unwrap();
    fs::write(root.join("Sources/agenttask/main.swift"), "print(1)").unwrap();
    fs::create_dir_all(root.join("Sources/MenuApp")).unwrap();
    fs::write(root.join("Sources/MenuApp/Views.swift"), "struct V {}").unwrap();
    fs::write(root.join("Sources/MenuApp/App.swift"), "import SwiftUI\n@main\nstruct MenuApp: App {}\n").unwrap();
    fs::create_dir_all(root.join("Tests/CoreTests")).unwrap();
    fs::write(root.join("Tests/CoreTests/CoreTests.swift"), "").unwrap();
    fs::write(root.join(".swiftlint.yml"), "").unwrap();

    let brief = scanner::scan(root);
    let m = brief.manifest.as_ref().expect("Package.swift is a manifest");
    assert_eq!(m.kind, "Package.swift");
    assert_eq!(m.name.as_deref(), Some("AgentTaskMonitor"));
    assert_eq!(m.version.as_deref(), Some("6.0"));
    assert_eq!(m.dependencies.get("swift-argument-parser").map(String::as_str), Some(">=1.3.0"));
    assert_eq!(m.products.len(), 2, "{:?}", m.products);
    assert!(m.products.iter().any(|p| p.name == "agenttask" && p.kind == "executable"));
    let names: Vec<(&str, &str, &str)> =
        m.targets.iter().map(|t| (t.name.as_str(), t.kind.as_str(), t.path.as_str())).collect();
    assert_eq!(
        names,
        vec![
            ("Core", "target", "Sources/Core"),
            ("agenttask", "executableTarget", "Sources/agenttask"),
            ("MenuApp", "executableTarget", "Sources/MenuApp"),
            ("CoreTests", "testTarget", "Tests/CoreTests"),
        ]
    );
    let cli = m.targets.iter().find(|t| t.name == "agenttask").unwrap();
    assert_eq!(cli.dependencies, vec!["swift-argument-parser/ArgumentParser", "Core"]);
    assert!(brief.entry_points.iter().any(|e| e.path == "Sources/agenttask/main.swift" && e.kind == "main"));
    assert!(brief.entry_points.iter().any(|e| e.path == "Sources/MenuApp/App.swift" && e.kind == "main"));
    assert!(brief.entry_points.iter().any(|e| e.path == "Tests/CoreTests" && e.kind == "test"));
    assert!(brief.entry_points.iter().any(|e| e.path == "Package.swift" && e.kind == "config"));
    assert_eq!(brief.tooling.build_tool.as_deref(), Some("swift build"));
    assert_eq!(brief.tooling.test_runner.as_deref(), Some("swift test"));
    assert_eq!(brief.tooling.linter.as_deref(), Some("swiftlint"));
    assert_eq!(brief.tooling.package_manager.as_deref(), Some("swiftpm"));

    // The project name comes from the manifest, not a bare `origin.git` remote.
    git_init(root);
    git(root, &["remote", "add", "origin", "/srv/git/origin.git"]);
    assert_eq!(knobyte::config::detect_project_name(root), "AgentTaskMonitor");
}

#[test]
fn project_name_ignores_bare_origin_remote() {
    let d = tempdir().unwrap();
    let root = d.path().join("my-tool");
    fs::create_dir_all(&root).unwrap();
    git_init(&root);
    git(&root, &["remote", "add", "origin", "/srv/git/origin.git"]);
    assert_eq!(knobyte::config::detect_project_name(&root), "my-tool");
}

// ---------------------------------------------------------------- setup, anchors, score

#[test]
fn fresh_setup_scores_100_and_is_idempotent() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::write(root.join("main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    fs::write(root.join(".cursorrules"), "# House rules\r\nBe kind.\r\n").unwrap();

    let out = knobyte(root, &["setup", "--tools", "claude,codex,cursor,windsurf,copilot,opencode", "--no-agent"], None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("COPY BELOW THIS LINE"), "setup never pauses on a pasted prompt");
    assert!(stdout.contains("Population pending"), "{}", stdout);
    assert!(!stdout.contains("Has population finished"));

    // Files setup writes per tool.
    for f in [
        "CLAUDE.md",
        "AGENTS.md",
        ".claude/skills/knobyte-inbox/SKILL.md",
        ".claude/skills/knobyte-inbox/.knobyte-managed.json",
        ".agents/skills/knobyte-relay/SKILL.md",
        ".windsurfrules",
        ".github/copilot-instructions.md",
        "opencode.json",
        ".mcp.json",
        ".cursor/mcp.json",
        ".vscode/mcp.json",
        ".codex/config.toml",
        ".knobyte/context/decisions.md",
        ".knobyte/context/setup.md",
        ".knobyte/patterns/INDEX.md",
        ".knobyte/SYNC.md",
    ] {
        assert!(root.join(f).exists(), "missing {}", f);
    }
    let cursor = fs::read_to_string(root.join(".cursorrules")).unwrap();
    assert!(cursor.starts_with("# House rules\r\nBe kind.\r\n\r\n<!-- knobyte-anchor:start -->\r\n"), "{:?}", cursor);
    let cfg: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join(".knobyte/config.json")).unwrap()).unwrap();
    assert_eq!(cfg["aiTools"].as_array().unwrap().len(), 6);
    let stack = fs::read_to_string(root.join(".knobyte/context/stack.md")).unwrap();
    assert!(stack.contains("status: in_flight") && stack.contains("last_updated: 20"));
    assert!(!stack.contains("status: draft"));

    // OpenCode: instructions and the MCP server share the root opencode.json.
    let oc: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join("opencode.json")).unwrap()).unwrap();
    assert!(oc["instructions"].as_array().unwrap().iter().any(|i| i == ".knobyte/AGENTS.md"));
    assert_eq!(oc["mcp"]["knobyte"]["type"], "local");
    // Windsurf's MCP configuration is user-level: never written without consent.
    assert!(!empty_home().join(".codeium").exists());
    assert!(stdout.contains("~/.codeium/windsurf/mcp_config.json"), "{}", stdout);

    // Docs still marked "to fill" are information, not drift.
    let check = knobyte(root, &["check", "--json"], None);
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(report["score"].as_f64(), Some(100.0), "{}", serde_json::to_string_pretty(&report["issues"]).unwrap());
    let pending: Vec<&serde_json::Value> = report["issues"].as_array().unwrap().iter().filter(|i| i["code"] == "POPULATION_PENDING").collect();
    assert_eq!(pending.len(), 7);
    assert!(pending.iter().all(|i| i["severity"] == "info"));
    assert!(report["issues"].as_array().unwrap().iter().all(|i| i["severity"] != "error"));

    // Second run: nothing changes.
    let files = ["CLAUDE.md", ".cursorrules", "opencode.json", ".knobyte/config.json", ".mcp.json", ".cursor/mcp.json", ".vscode/mcp.json", ".codex/config.toml"];
    let snapshot = |p: &str| fs::read(root.join(p)).unwrap();
    let before: Vec<Vec<u8>> = files.iter().map(|p| snapshot(p)).collect();
    let again = knobyte(root, &["setup", "--no-agent"], None);
    assert!(again.status.success());
    let after: Vec<Vec<u8>> = files.iter().map(|p| snapshot(p)).collect();
    assert_eq!(before, after);
    assert!(String::from_utf8_lossy(&again.stdout).contains("Using configured AI tools"));
}

fn mkdirs(base: &Path, dirs: &[&str]) {
    for d in dirs {
        fs::create_dir_all(base.join(d)).unwrap();
    }
}

/// A small Rust project where `parse_config` is the obviously central function.
fn write_central_project(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
    fs::write(root.join("src/config.rs"), "pub fn parse_config(s: &str) -> usize {\n    s.len()\n}\n").unwrap();
    for name in ["a", "b", "c"] {
        fs::write(
            root.join(format!("src/{}.rs", name)),
            format!("use crate::config::parse_config;\n\npub fn run_{}() -> usize {{\n    parse_config(\"{}\")\n}}\n", name, name),
        )
        .unwrap();
    }
    fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\npub mod c;\npub mod config;\n").unwrap();
}

/// No `--tools`: setup detects the developer's tools (here Cursor from `~/.cursor` and Codex
/// from a CLI on PATH), wires each one including its MCP server, indexes in one pass, does not
/// pause for population and ends with a proof summary naming a real central symbol.
#[cfg(unix)]
#[test]
fn setup_detects_tools_wires_mcp_and_finishes_without_pausing() {
    let d = tempdir().unwrap();
    let root = d.path().join("proj");
    let home = d.path().join("home");
    mkdirs(&home, &[".cursor"]);
    let fake = d.path().join("bin");
    write_script(&fake, "codex", "#!/bin/sh\ntouch \"$HOME/codex-ran\"\nexit 1\n");
    fs::create_dir_all(&root).unwrap();
    git_init(&root);
    write_central_project(&root);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);

    let out = knobyte_with(&root, &["setup"], Some(&fake), &home);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}\n{}", stdout, String::from_utf8_lossy(&out.stderr));
    assert!(!home.join("codex-ran").exists(), "never launched without consent");
    let cfg: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join(".knobyte/config.json")).unwrap()).unwrap();
    assert_eq!(cfg["aiTools"], serde_json::json!(["cursor", "codex"]), "{}", stdout);
    assert!(stdout.contains("Detected:") && stdout.contains("~/.cursor") && stdout.contains("codex on PATH"), "{}", stdout);
    for f in [".cursorrules", ".cursor/mcp.json", "AGENTS.md", ".agents/skills/knobyte-inbox/SKILL.md", ".codex/config.toml"] {
        assert!(root.join(f).exists(), "missing {}", f);
    }
    assert!(!root.join(".mcp.json").exists() && !root.join("CLAUDE.md").exists());
    let toml = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    assert!(toml.contains("[mcp_servers.knobyte]") && toml.contains("args = [\"mcp\", \"--stdio\", \"--profile\", \"core\"]"), "{}", toml);
    let cursor: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert_eq!(cursor["mcpServers"]["knobyte"]["command"], bin().to_string_lossy().as_ref(), "not on PATH: absolute path");

    // One indexing display, then the proof.
    for needle in ["[1/4] scan", "[2/4] code graph", "[3/4] vector index", "[4/4] wiki index", "Setup summary", "Vector index", "ready (hashed-v1", "Drift score", "100/100", "0/7 populated"] {
        assert!(stdout.contains(needle), "missing {:?}:\n{}", needle, stdout);
    }
    assert!(stdout.contains("Try asking your agent: \"Use Knobyte to explain how parse_config works.\""), "{}", stdout);
    // The commit is offered last and nothing is committed without consent.
    let commit_at = stdout.find("git commit -m").unwrap();
    assert!(commit_at > stdout.find("Try asking your agent").unwrap());
    assert!(stdout.contains(".cursor/mcp.json .codex/config.toml"), "MCP files are part of the checkpoint: {}", stdout);
    assert_eq!(git(&root, &["log", "--oneline"]).lines().count(), 1);

    // Population is pending: markers stay, every instruction surface says what to do next.
    assert!(!knobyte::setup::is_scaffold_populated(&root.join(".knobyte")));
    assert!(root.join(".knobyte/local/setup-pending").exists());
    assert!(fs::read_to_string(root.join("AGENTS.md")).unwrap().contains("knobyte setup --finish"));
    assert!(fs::read_to_string(root.join(".cursorrules")).unwrap().contains("knobyte setup --finish"));
    assert!(fs::read_to_string(root.join(".knobyte/AGENTS.md")).unwrap().contains("Population pending"));
    assert!(fs::read_to_string(root.join(".knobyte/ROUTER.md")).unwrap().contains("knobyte setup --finish"));
    let prompt = knobyte_with(&root, &["setup", "--print-prompt"], None, &home);
    assert!(String::from_utf8_lossy(&prompt.stdout).contains("knobyte setup --finish"));

    // The MCP server tells the agent too.
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(["mcp", "--stdio", "--root", root.to_str().unwrap()])
        .current_dir(d.path())
        .env("HOME", &home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"knobyte_session_start\",\"arguments\":{}}}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Knobyte population is pending"), "{}", text);
    assert!(text.contains("population_pending\\\": true"), "{}", text);
}

#[test]
fn nothing_detected_falls_back_to_agents_and_claude_md() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let out = knobyte(root, &["setup", "--skip-graph"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", stdout);
    assert!(stdout.contains("No AI tools detected; writing AGENTS.md and CLAUDE.md"), "{}", stdout);
    assert!(root.join("AGENTS.md").exists() && root.join("CLAUDE.md").exists());
    assert!(!root.join(".mcp.json").exists() && !root.join(".codex").exists(), "nothing detected: no MCP files");
    // `--tools none` still records an explicit "no tools" decision.
    let d2 = tempdir().unwrap();
    git_init(d2.path());
    assert!(knobyte(d2.path(), &["setup", "--tools", "none", "--skip-graph"], None).status.success());
    assert!(!d2.path().join("AGENTS.md").exists() && !d2.path().join("CLAUDE.md").exists());
}

#[test]
fn no_mcp_skips_registration_and_tools_flag_overrides_detection() {
    let d = tempdir().unwrap();
    let root = d.path().join("p");
    let home = d.path().join("home");
    mkdirs(&home, &[".cursor", ".codex"]);
    fs::create_dir_all(&root).unwrap();
    git_init(&root);
    let out = knobyte_with(&root, &["setup", "--tools", "claude", "--no-mcp", "--skip-graph"], None, &home);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Skipping MCP server registration"));
    assert!(root.join("CLAUDE.md").exists() && !root.join(".mcp.json").exists());
    assert!(!root.join(".cursorrules").exists(), "--tools overrides detection");
}

/// Windsurf's MCP file is user-level: setup prints the snippet and writes it only with
/// `--global-mcp`, merging into the existing file.
#[test]
fn windsurf_user_config_needs_explicit_flag() {
    let d = tempdir().unwrap();
    let root = d.path().join("p");
    let home = d.path().join("home");
    mkdirs(&home, &[".codeium/windsurf"]);
    let file = home.join(".codeium/windsurf/mcp_config.json");
    let original = "{\n  \"mcpServers\": {\n    \"other\": { \"command\": \"other-server\" }\n  }\n}\n";
    fs::write(&file, original).unwrap();
    fs::create_dir_all(&root).unwrap();
    git_init(&root);

    let out = knobyte_with(&root, &["setup", "--tools", "windsurf", "--skip-graph"], None, &home);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", stdout);
    assert_eq!(fs::read_to_string(&file).unwrap(), original, "never touched without consent");
    assert!(stdout.contains("~/.codeium/windsurf/mcp_config.json") && stdout.contains("--global-mcp"), "{}", stdout);
    assert!(stdout.contains("\"--root\""), "snippet printed: {}", stdout);

    let out = knobyte_with(&root, &["setup", "--tools", "windsurf", "--global-mcp", "--skip-graph"], None, &home);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(v["mcpServers"]["other"]["command"], "other-server");
    assert_eq!(v["mcpServers"]["knobyte"]["args"][4], "--root");
    let again = fs::read_to_string(&file).unwrap();
    assert!(knobyte_with(&root, &["setup", "--tools", "windsurf", "--global-mcp", "--skip-graph"], None, &home).status.success());
    assert_eq!(fs::read_to_string(&file).unwrap(), again, "idempotent");
    let paths = git(&root, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(!paths.contains("mcp_config"), "user-level files are not in the repository");
}

/// A repository that ignores `.vscode/`: the Copilot MCP file is written but left out of the
/// commit (and reported), so the printed `git add` command and `--commit` both work.
#[test]
fn gitignored_mcp_file_is_left_out_of_the_commit() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::write(root.join(".gitignore"), "target/\n.vscode/\n").unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    let out = knobyte(root, &["setup", "--tools", "claude,copilot", "--no-agent", "--skip-graph"], None);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}", stdout);
    assert!(root.join(".vscode/mcp.json").exists(), "still written for local use");
    assert!(stdout.contains("Not committed") && stdout.contains("git add -f .vscode/mcp.json"), "{}", stdout);
    let add_line = stdout.lines().find(|l| l.trim_start().starts_with("git add -- ")).unwrap().trim().to_string();
    assert!(!add_line.contains(".vscode"), "{}", add_line);
    // The printed command works as shown.
    let args: Vec<&str> = add_line.split_whitespace().skip(1).collect();
    git(root, &args);
    git(root, &["reset", "-q"]);

    let out = knobyte(root, &["setup", "--tools", "claude,copilot", "--no-agent", "--skip-graph", "--commit"], None);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}", stdout);
    assert!(stdout.contains("Committed:"), "{}", stdout);
    let files = git(root, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(files.contains(".mcp.json") && files.contains("CLAUDE.md") && !files.contains(".vscode"), "{}", files);
}

/// `--finish` after population: re-scan, finalize with baselines, clear the pending marker.
#[test]
fn finish_captures_baselines_after_population() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::write(root.join("lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
    assert!(knobyte(root, &["setup", "--tools", "claude", "--no-agent"], None).status.success());
    assert!(root.join(".knobyte/local/setup-pending").exists());

    // `--finish` before population reports what is still pending and succeeds.
    let early = knobyte(root, &["setup", "--finish"], None);
    assert!(early.status.success());
    assert!(String::from_utf8_lossy(&early.stdout).contains("Population is still pending"));

    populate_scaffold(root);
    let arch = root.join(".knobyte/context/architecture.md");
    let mut text = fs::read_to_string(&arch).unwrap();
    text.push_str("\nAddition lives in lib.rs.\n<!-- kb-ground: function:lib.rs:add -->\n");
    fs::write(&arch, &text).unwrap();
    let out = knobyte(root, &["setup", "--finish"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}\n{}", stdout, String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("Finishing setup"), "{}", stdout);
    assert!(stdout.contains("[2/4] code graph"), "re-scanned: {}", stdout);
    assert!(stdout.contains("Captured 1 grounding baseline"), "{}", stdout);
    assert!(stdout.contains("7/7 populated") && !stdout.contains("population pending"), "{}", stdout);
    assert!(stdout.contains("Try asking your agent: \"Use Knobyte to explain how add works.\""), "{}", stdout);
    assert!(fs::read_to_string(&arch).unwrap().contains("<!-- kb-ground: function:lib.rs:add #"));
    assert!(!root.join(".knobyte/local/setup-pending").exists());
    let check = knobyte(root, &["check", "--json"], None);
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert!(!report["issues"].as_array().unwrap().iter().any(|i| i["code"] == "POPULATION_PENDING"));
}

/// Setup's own templates carry explicit ids, types, lifecycle states and `relations`: the
/// finalize migration has nothing to rewrite, catch-up shows no migration noise, and the
/// starter docs are not orphaned once promoted.
#[test]
fn fresh_populated_setup_migrates_nothing_and_orphans_nothing() {
    for mode in ["code-repo", "agent-memory"] {
        let d = tempdir().unwrap();
        let root = d.path();
        git_init(root);
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let out = knobyte(root, &["setup", "--mode", mode, "--tools", "claude", "--no-agent", "--skip-graph"], None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let plan = knobyte(root, &["wiki", "migrate", "--json"], None);
        let v: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
        assert_eq!(v["data"]["plan"]["items"].as_array().map(Vec::len), Some(0), "{}: {}", mode, v["data"]["plan"]["items"]);

        populate_scaffold(root);
        let finish = knobyte(root, &["setup", "--no-agent", "--skip-graph"], None);
        assert!(finish.status.success(), "{}", String::from_utf8_lossy(&finish.stderr));
        let log = fs::read_to_string(root.join(".knobyte/events/operations.jsonl")).unwrap_or_default();
        assert!(!log.contains("wiki migrate"), "{}: finalize migrated: {}", mode, log);

        for f in walkdir::WalkDir::new(root.join(".knobyte/context")).into_iter().flatten() {
            if f.path().extension().is_some_and(|e| e == "md") {
                let text = fs::read_to_string(f.path()).unwrap().replace("status: in_flight", "status: promoted");
                fs::write(f.path(), text).unwrap();
            }
        }
        let validate = knobyte(root, &["wiki", "validate"], None);
        let text = String::from_utf8_lossy(&validate.stdout);
        assert!(!text.contains("ORPHANED_ENTITY"), "{}: {}", mode, text);
        let check = knobyte(root, &["check", "--json"], None);
        let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
        let issues = report["issues"].as_array().unwrap();
        assert!(!issues.iter().any(|i| i["code"] == "DEAD_EDGE"), "{}: {:?}", mode, issues);
    }
}

#[test]
fn setup_without_tools_records_decision_and_scores_100() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    let out = knobyte(root, &["setup", "--tools", "none", "--no-agent", "--skip-graph"], None);
    assert!(out.status.success());
    assert!(!root.join("CLAUDE.md").exists());
    let check = knobyte(root, &["check", "--json"], None);
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert!(!report["issues"].as_array().unwrap().iter().any(|i| i["code"] == "SCAFFOLD_ORPHANED"));
}

#[test]
fn malformed_managed_block_is_a_conflict_and_preserved() {
    let d = tempdir().unwrap();
    let root = d.path();
    let broken = "# Mine\n<!-- knobyte-agent:skills:start -->\nhalf a block\n";
    fs::write(root.join("CLAUDE.md"), broken).unwrap();
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    let report = sync_skills(&config, Some("claude"), false).unwrap();
    assert!(report.conflicted);
    assert!(report.warnings.iter().any(|w| w.code == "malformed-instruction-markers"));
    assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), broken);

    let out = knobyte(root, &["setup", "--tools", "claude", "--no-agent", "--skip-graph"], None);
    assert!(!out.status.success(), "setup stops on agent-asset conflicts");
    assert_eq!(fs::read_to_string(root.join("CLAUDE.md")).unwrap(), broken);
}

#[test]
fn managed_block_replaces_only_its_own_bytes() {
    let d = tempdir().unwrap();
    let root = d.path();
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    fs::write(
        root.join("AGENTS.md"),
        "intro\r\n<!-- knobyte-agent:skills:start -->\r\nold\r\n<!-- knobyte-agent:skills:end -->\r\noutro\r\n",
    )
    .unwrap();
    let r = sync_skills(&config, Some("codex"), false).unwrap();
    assert!(r.actions.iter().any(|a| a.skill_name == "instructions" && a.action == "update"));
    let content = fs::read_to_string(root.join("AGENTS.md")).unwrap();
    assert!(content.starts_with("intro\r\n<!-- knobyte-agent:skills:start -->\r\n## Knobyte agent skills\r\n"));
    assert!(content.ends_with("<!-- knobyte-agent:skills:end -->\r\noutro\r\n"));
    assert!(!content.replace("\r\n", "").contains('\n'), "CRLF preserved");
    let again = sync_skills(&config, Some("codex"), false).unwrap();
    assert!(again.actions.iter().all(|a| a.action == "unchanged"));
}

#[test]
fn skill_files_are_never_clobbered() {
    let d = tempdir().unwrap();
    let root = d.path();
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    sync_skills(&config, Some("claude"), false).unwrap();
    let extra = root.join(".claude/skills/knobyte-relay/notes.md");
    fs::write(&extra, "mine").unwrap();
    let r = sync_skills(&config, Some("claude"), false).unwrap();
    assert!(r.conflicted);
    assert_eq!(fs::read_to_string(&extra).unwrap(), "mine");
    let r = sync_skills_with(&config, Some("claude"), SkillSyncOptions { backup_conflicts: true, ..Default::default() }).unwrap();
    assert!(!r.conflicted);
    assert!(!extra.exists());
    let backups: Vec<_> = fs::read_dir(config.local_dir().join("skill-backups")).unwrap().collect();
    assert_eq!(backups.len(), 1);
    let backed = backups[0].as_ref().unwrap().path().join("notes.md");
    assert_eq!(fs::read_to_string(backed).unwrap(), "mine");
}

// ------------------------------------------------------------------------------ launcher

#[cfg(unix)]
fn launch_opts(root: &Path, fake_bin: &Path, timeout: Option<Duration>) -> LaunchOptions {
    LaunchOptions {
        cwd: root.to_path_buf(),
        private_dir: root.join(".knobyte/local"),
        timeout,
        path_env: Some(OsString::from(format!("{}:/usr/bin:/bin", fake_bin.display()))),
        allow_non_git: true,
    }
}

#[cfg(unix)]
#[test]
fn launcher_streams_fake_claude_and_cleans_up_prompt() {
    let d = tempdir().unwrap();
    let root = d.path();
    fs::create_dir_all(root.join(".knobyte/local")).unwrap();
    let fake = root.join("fakebin");
    write_script(
        &fake,
        "claude",
        r#"#!/bin/sh
printf '%s\n' "$@" > args.txt
prompt=$(sed -n 's/.*`\(.*prompt\.md\)`.*/\1/p' args.txt | head -n 1)
cp "$prompt" seen-prompt.md
echo '{"type":"system","subtype":"init"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Working on it"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":".knobyte/context/stack.md"}}]}}'
echo '{"type":"result","subtype":"success","is_error":false}'
"#,
    );
    let mut events = Vec::new();
    let outcome = run_agent(AgentTool::Claude, "POPULATE THE SCAFFOLD", &launch_opts(root, &fake, None), &mut |e| events.push(e.clone()));
    assert!(outcome.completed, "{:?}", outcome.failure);
    assert!(events.contains(&AgentEvent::Assistant { text: "Working on it".into() }));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Tool { detail, .. } if detail.contains("stack.md"))));
    let args = fs::read_to_string(root.join("args.txt")).unwrap();
    assert!(args.contains("--allowedTools") && args.contains("Bash(knobyte graph scope:*)"));
    assert!(args.contains("stream-json"));
    assert_eq!(fs::read_to_string(root.join("seen-prompt.md")).unwrap(), "POPULATE THE SCAFFOLD");
    // The private prompt file is removed after the session.
    let sessions = root.join(".knobyte/local/agent-sessions");
    assert_eq!(fs::read_dir(&sessions).map(|r| r.count()).unwrap_or(0), 0);
}

#[cfg(unix)]
#[test]
fn launcher_reports_protocol_and_auth_failures() {
    let d = tempdir().unwrap();
    let root = d.path();
    let fake = root.join("fakebin");
    write_script(&fake, "claude", "#!/bin/sh\necho '{\"type\":\"system\",\"subtype\":\"init\"}'\nexit 0\n");
    let o = run_agent(AgentTool::Claude, "p", &launch_opts(root, &fake, None), &mut |_| {});
    assert_eq!(o.failure, Some(LaunchFailure::Protocol));
    write_script(&fake, "codex", "#!/bin/sh\necho 'Error: not logged in' >&2\nexit 1\n");
    let o = run_agent(AgentTool::Codex, "p", &launch_opts(root, &fake, None), &mut |_| {});
    assert_eq!(o.failure, Some(LaunchFailure::Authentication));
    let empty = root.join("empty-bin");
    fs::create_dir_all(&empty).unwrap();
    let o = run_agent(AgentTool::Claude, "p", &launch_opts(root, &empty, None), &mut |_| {});
    assert_eq!(o.failure, Some(LaunchFailure::Launch));
}

#[cfg(unix)]
#[test]
fn launcher_timeout_kills_the_whole_process_tree() {
    let d = tempdir().unwrap();
    let root = d.path();
    let fake = root.join("fakebin");
    write_script(
        &fake,
        "codex",
        "#!/bin/sh\n[ -n \"$KNOBYTE_TEST_PREWARM\" ] && exit 0\nsleep 300 &\necho $! > grandchild.pid\necho '{\"type\":\"thread.started\"}'\nsleep 300\n",
    );
    // macOS scans a freshly written executable on its first launch, which can take longer than
    // the launch timeout on a loaded machine. Run it once up front so the timed run measures the
    // launcher, not the scan.
    let _ = Command::new(fake.join("codex"))
        .env("KNOBYTE_TEST_PREWARM", "1")
        .current_dir(root)
        .status();
    let started = std::time::Instant::now();
    let o = run_agent(AgentTool::Codex, "p", &launch_opts(root, &fake, Some(Duration::from_secs(8))), &mut |_| {});
    assert_eq!(o.failure, Some(LaunchFailure::Timeout));
    assert!(started.elapsed() < Duration::from_secs(30));
    let raw = fs::read_to_string(root.join("grandchild.pid")).unwrap_or_default();
    let pid: i32 = raw.trim().parse().unwrap_or_else(|_| panic!("pid file {:?}, outcome {:?}", raw, o));
    // The grandchild was in the agent's process group and must be gone.
    let mut alive = true;
    for _ in 0..50 {
        let st = Command::new("kill").args(["-0", &pid.to_string()]).output().unwrap();
        if !st.status.success() {
            alive = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive, "grandchild {} survived", pid);
}

#[cfg(unix)]
#[test]
fn setup_launches_fake_agent_only_with_explicit_flag_then_finalizes() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::write(root.join("lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
    let fake = root.join(".fakebin");
    write_script(
        &fake,
        "claude",
        r#"#!/bin/sh
touch launched.txt
for f in .knobyte/AGENTS.md .knobyte/ROUTER.md .knobyte/context/*.md; do
  grep -v 'knobyte:populate' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
done
echo '{"type":"system","subtype":"init"}'
echo '{"type":"result","subtype":"success","is_error":false}'
"#,
    );

    // Non-interactive without the flag: never launched, prompt printed instead.
    let out = knobyte(root, &["setup", "--tools", "claude"], Some(&fake));
    assert!(out.status.success());
    assert!(!root.join("launched.txt").exists(), "must not launch without --launch-agent");
    assert!(String::from_utf8_lossy(&out.stdout).contains("pass --launch-agent"));

    // CI without the flag: never launched either.
    let mut ci = Command::new(bin());
    let mut paths = vec![fake.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    ci.args(["setup"]).current_dir(root).env("CI", "true").env("PATH", std::env::join_paths(&paths).unwrap()).env_remove("KNOBYTE_NO_AGENT_LAUNCH");
    assert!(ci.output().unwrap().status.success());
    assert!(!root.join("launched.txt").exists());

    // Explicit consent: the fake agent populates, setup finalizes.
    let out = knobyte(root, &["setup", "--launch-agent"], Some(&fake));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}\n{}", stdout, String::from_utf8_lossy(&out.stderr));
    assert!(root.join("launched.txt").exists());
    assert!(stdout.contains("--allowedTools"), "exact command shown before launching");
    assert!(stdout.contains("Wiki ready"), "{}", stdout);
    assert!(stdout.contains("git commit -m"), "commit checkpoint commands printed");
    let log = Command::new("git").args(["log", "--oneline"]).current_dir(root).output().unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).trim().is_empty(), "no commit without confirmation");
    assert!(knobyte::setup::is_scaffold_populated(&root.join(".knobyte")));
}

#[cfg(unix)]
#[test]
fn sync_repair_loop_with_fake_agent_shows_score_delta() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    assert!(knobyte(root, &["setup", "--tools", "claude", "--no-agent"], None).status.success());
    let setup_md = root.join(".knobyte/context/setup.md");
    let original = fs::read_to_string(&setup_md).unwrap();
    fs::write(&setup_md, format!("{}\nRun `src/missing_tool.rs` first.\n", original)).unwrap();

    // Without consent the loop only prints the prompt.
    let out = knobyte(root, &["sync"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("COPY BELOW THIS LINE"));

    let fake = root.join(".fakebin");
    write_script(
        &fake,
        "claude",
        r#"#!/bin/sh
grep -v 'missing_tool' .knobyte/context/setup.md > s.tmp && mv s.tmp .knobyte/context/setup.md
echo '{"type":"system","subtype":"init"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Removed the dead path"}]}}'
echo '{"type":"result","subtype":"success","is_error":false}'
"#,
    );
    let out = knobyte(root, &["sync", "--launch-agent", "--accept"], Some(&fake));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", stdout);
    assert!(stdout.contains("Drift score: 90 -> 100/100 (+10)"), "{}", stdout);
    assert!(stdout.contains("Removed the dead path"));
}

// ---------------------------------------------------------------------------- small cmds

#[test]
fn completion_scripts_cover_commands() {
    let d = tempdir().unwrap();
    for shell in ["bash", "zsh", "fish"] {
        let out = knobyte(d.path(), &["completion", shell], None);
        assert!(out.status.success());
        let s = String::from_utf8_lossy(&out.stdout);
        for cmd in ["setup", "sync", "graph", "watch", "tui", "completion", "logging"] {
            assert!(s.contains(cmd), "{} completion lacks {}", shell, cmd);
        }
    }
    assert!(String::from_utf8_lossy(&knobyte(d.path(), &["completion", "bash"], None).stdout).contains("rebuild"));
    assert!(!knobyte(d.path(), &["completion", "tcsh"], None).status.success());
}

#[test]
fn logging_watch_pattern_update_and_doctor() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    assert!(knobyte(root, &["setup", "--tools", "claude", "--no-agent", "--skip-graph"], None).status.success());

    let out = knobyte(root, &["logging", "--json"], None);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["mode"], "significant");
    let out = knobyte(root, &["logging", "manual", "--expected-revision", "none", "--json"], None);
    assert!(out.status.success());
    let out = knobyte(root, &["logging", "checkpoints", "--expected-revision", "none"], None);
    assert_eq!(out.status.code(), Some(4));

    let out = knobyte(root, &["watch"], None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let hook = root.join(".git/hooks/post-commit");
    assert!(fs::read_to_string(&hook).unwrap().contains("check --quiet"));
    assert!(knobyte(root, &["watch", "--uninstall"], None).status.success());
    assert!(!hook.exists());

    assert!(knobyte(root, &["pattern", "add", "add-endpoint"], None).status.success());
    assert!(!knobyte(root, &["pattern", "add", "add-endpoint"], None).status.success());
    assert!(!knobyte(root, &["pattern", "add", "bad name"], None).status.success());
    let index = fs::read_to_string(root.join(".knobyte/patterns/INDEX.md")).unwrap();
    assert!(index.contains("[add-endpoint.md](add-endpoint.md)"));

    // update: refreshes infrastructure files, never populated content.
    fs::write(root.join(".knobyte/SYNC.md"), "stale copy").unwrap();
    let arch = root.join(".knobyte/context/architecture.md");
    fs::write(&arch, "my architecture").unwrap();
    let out = knobyte(root, &["update", "--json"], None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_ne!(fs::read_to_string(root.join(".knobyte/SYNC.md")).unwrap(), "stale copy");
    assert_eq!(fs::read_to_string(&arch).unwrap(), "my architecture");

    let out = knobyte(root, &["doctor", "--json"], None);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["drift_score"].is_number());
    assert_eq!(v["graph_status"], "missing");
    assert!(v["next_steps"].as_array().unwrap().iter().any(|s| s.as_str().unwrap().contains("graph")));
    assert_eq!(out.status.code().unwrap(), v["exit_code"].as_i64().unwrap() as i32);

    let out = knobyte(root, &["capabilities", "--json"], None);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["repository"]["initializationState"], "needs_population");
    assert!(v["exitCodes"].as_array().unwrap().len() >= 6);
    assert!(v["commandsByKind"]["apply"].as_array().unwrap().iter().any(|c| c["id"] == "setup"));
}

#[cfg(unix)]
#[test]
fn graph_ground_agent_needs_explicit_consent_to_launch() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::write(root.join("lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
    let out = knobyte(root, &["setup", "--tools", "claude", "--no-agent"], None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let fake = root.join(".fakebin");
    write_script(
        &fake,
        "claude",
        "#!/bin/sh\ntouch launched.txt\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n",
    );

    // `--agent` alone (non-interactive): the command is not launched; the prompt is printed.
    let out = knobyte(root, &["graph", "ground", "--agent"], Some(&fake));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!root.join("launched.txt").exists(), "--agent alone must not launch: {}", stdout);
    assert!(stdout.contains("graph ground --rebaseline"), "prompt fallback expected: {}", stdout);

    // `--launch-agent` is the explicit consent: the command is shown, then launched.
    let out = knobyte(root, &["graph", "ground", "--agent", "--launch-agent"], Some(&fake));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(root.join("launched.txt").exists(), "--launch-agent should launch: {}", stdout);
    assert!(stdout.contains("will run in"), "launch preview expected: {}", stdout);
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Strip the populate markers from the scaffold, as a populating agent does.
fn populate_scaffold(root: &Path) {
    for entry in walkdir::WalkDir::new(root.join(".knobyte")).into_iter().flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "md") {
            let text = fs::read_to_string(p).unwrap();
            if text.contains("knobyte:populate") {
                let kept: Vec<&str> = text.lines().filter(|l| !l.contains("knobyte:populate")).collect();
                fs::write(p, kept.join("\n") + "\n").unwrap();
            }
        }
    }
}

/// Re-running `knobyte setup` in a clone of a populated repository leaves tracked files
/// alone: it reports groundings without a baseline instead of writing them, rebuilds only
/// the local indexes, and writes baselines only with `--capture-baselines`. Finishing a setup
/// that paused at population still captures baselines.
#[test]
fn setup_rerun_on_populated_clone_does_not_modify_tracked_files() {
    let d = tempdir().unwrap();
    let origin = d.path().join("origin");
    fs::create_dir_all(&origin).unwrap();
    git_init(&origin);
    fs::write(origin.join("lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n").unwrap();

    // Fresh setup pauses at population; finishing it captures the baseline of `add`.
    assert!(knobyte(&origin, &["setup", "--tools", "claude", "--no-agent"], None).status.success());
    populate_scaffold(&origin);
    let arch = origin.join(".knobyte/context/architecture.md");
    let mut text = fs::read_to_string(&arch).unwrap();
    text.push_str("\nAddition lives in lib.rs.\n<!-- kb-ground: function:lib.rs:add -->\n");
    fs::write(&arch, &text).unwrap();
    let out = knobyte(&origin, &["setup", "--no-agent"], None);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}\n{}", stdout, String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("Captured 1 grounding baseline"), "{}", stdout);
    assert!(fs::read_to_string(&arch).unwrap().contains("<!-- kb-ground: function:lib.rs:add #"));

    // A teammate later grounds `sub` without capturing a baseline, and commits.
    let mut text = fs::read_to_string(&arch).unwrap();
    text.push_str("\nSubtraction too.\n<!-- kb-ground: function:lib.rs:sub -->\n");
    fs::write(&arch, &text).unwrap();
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-qm", "memory"]);

    let clone = d.path().join("clone");
    git(d.path(), &["clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()]);
    git_init(&clone);
    let out = knobyte(&clone, &["setup", "--no-agent"], None);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}\n{}", stdout, String::from_utf8_lossy(&out.stderr));
    assert_eq!(git(&clone, &["status", "--porcelain"]), "", "tracked files changed:\n{}", stdout);
    assert!(
        stdout.contains("1 grounding has no committed baseline — run `knobyte graph ground --rebaseline` to capture them"),
        "{}",
        stdout
    );
    assert!(clone.join(".knobyte/wiki.db").exists() && clone.join(".knobyte/graph.db").exists());
    assert!(!stdout.contains("git commit -m"), "no commit checkpoint on a re-run: {}", stdout);
    assert!(stdout.contains(".mcp.json already registers the Knobyte MCP server"), "{}", stdout);

    // A repository set up before MCP registration existed: joining still writes nothing and
    // says how to add it; detected tools on the joiner's machine change nothing either.
    git(&clone, &["rm", "-q", ".mcp.json"]);
    git(&clone, &["commit", "-qm", "older setup"]);
    let joiner_home = d.path().join("joiner-home");
    fs::create_dir_all(joiner_home.join(".cursor")).unwrap();
    let out = knobyte_with(&clone, &["setup", "--no-agent"], None, &joiner_home);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}", stdout);
    assert_eq!(git(&clone, &["status", "--porcelain"]), "", "tracked files changed:\n{}", stdout);
    assert!(!clone.join(".mcp.json").exists() && !clone.join(".cursorrules").exists());
    assert!(stdout.contains("Would create .mcp.json") && stdout.contains("knobyte setup --tools claude"), "{}", stdout);

    // Explicit opt-in writes the missing baseline.
    let out = knobyte(&clone, &["setup", "--no-agent", "--capture-baselines"], None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(git(&clone, &["status", "--porcelain"]).contains(".knobyte/context/architecture.md"));
    assert!(fs::read_to_string(clone.join(".knobyte/context/architecture.md"))
        .unwrap()
        .contains("<!-- kb-ground: function:lib.rs:sub #"));
}
