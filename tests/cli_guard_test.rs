//! CLI regression tests: project discovery guard, log/timeline validation, capabilities and
//! heartbeat JSON semantics, broken pipes, argument validation and plain error output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::{tempdir, TempDir};

use knobyte::config::KnobyteConfig;
use knobyte::events::{query_timeline_files, read_events, TimelineFilter, MAX_TIMELINE_OUTPUT_BYTES, MAX_TIMELINE_READ_ENTRIES};

fn kb(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KNOBYTE_NO_AGENT_LAUNCH", "1")
        .env_remove("CI")
        .output()
        .unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn git_init(dir: &Path) {
    let run = |args: &[&str]| {
        Command::new("git").args(args).current_dir(dir).env("HOME", dir).output().unwrap();
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "dev@example.com"]);
    run(&["config", "user.name", "Dev"]);
}

/// Entries of `dir` (names only), to prove a refused command created nothing.
fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
    v.sort();
    v
}

/// A git repository with a complete scaffold.
fn project() -> (TempDir, PathBuf) {
    let d = tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    git_init(&root);
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    knobyte::setup::run_setup(&config, "code-repo", false).unwrap();
    assert!(root.join(".knobyte/ROUTER.md").is_file());
    (d, root)
}

// ------------------------------------------------------------------- project discovery guard

#[test]
fn commands_outside_a_project_refuse_and_create_nothing() {
    let d = tempdir().unwrap();
    let root = d.path();
    let before = listing(root);
    for args in [
        vec!["skills", "sync"],
        vec!["skills", "sync", "--tool", "claude"],
        vec!["log", "hello"],
        vec!["pattern", "add", "thing"],
        vec!["logging", "manual"],
        vec!["timeline"],
        vec!["heartbeat"],
        vec!["doctor"],
        vec!["check"],
        vec!["export"],
        vec!["member", "list"],
        vec!["watch"],
    ] {
        let o = kb(root, &args);
        assert_eq!(o.status.code(), Some(3), "{:?}: {}", args, stderr(&o));
        assert!(stderr(&o).contains("No git repository found"), "{:?}: {}", args, stderr(&o));
        assert_eq!(listing(root), before, "{:?} created files", args);
    }
    for f in ["CLAUDE.md", "AGENTS.md", ".claude", ".agents", ".knobyte"] {
        assert!(!root.join(f).exists(), "{} was created", f);
    }
    // Commands that need no project still run.
    assert!(kb(root, &["commands"]).status.success());
    assert!(kb(root, &["completion", "bash"]).status.success());
    assert!(kb(root, &["init", "--json"]).status.success());
}

#[test]
fn git_repo_without_scaffold_says_run_setup() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    let o = kb(root, &["log", "hello"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(stderr(&o).contains("No .knobyte/ scaffold found — run `knobyte setup`"), "{}", stderr(&o));
    assert!(!root.join(".knobyte").exists());

    let o = kb(root, &["skills", "sync", "--json"]);
    assert_eq!(o.status.code(), Some(3));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "SKILL_SYNC_FAILED");
    assert!(!root.join("CLAUDE.md").exists() && !root.join(".claude").exists());
}

#[test]
fn empty_scaffold_is_incomplete_not_a_perfect_score() {
    let d = tempdir().unwrap();
    let root = d.path();
    git_init(root);
    fs::create_dir(root.join(".knobyte")).unwrap();
    for args in [vec!["check"], vec!["check", "--json"], vec!["doctor"], vec!["log", "x"]] {
        let o = kb(root, &args);
        assert_eq!(o.status.code(), Some(3), "{:?}", args);
        assert!(stderr(&o).contains("incomplete") && stderr(&o).contains("knobyte setup"), "{}", stderr(&o));
        assert!(!stdout(&o).contains("100/100"));
    }
    assert!(listing(&root.join(".knobyte")).is_empty(), "nothing written into the empty scaffold");
}

#[test]
fn inside_the_scaffold_directory_is_refused() {
    let (_d, root) = project();
    let o = kb(&root.join(".knobyte/context"), &["log", "x"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(stderr(&o).contains("inside the .knobyte/ directory"), "{}", stderr(&o));
}

#[test]
fn skills_sync_without_selected_agents_refuses_with_envelope() {
    let (_d, root) = project();
    let o = kb(&root, &["skills", "sync"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("No supported agent is selected"), "{}", stderr(&o));
    assert!(!root.join("CLAUDE.md").exists() && !root.join(".claude").exists());

    let o = kb(&root, &["skills", "sync", "--json"]);
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["error"]["code"], "SKILL_SYNC_FAILED");
    assert_eq!(v["ok"], false);

    let o = kb(&root, &["skills", "sync", "--tool", "claude", "--dry-run", "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["ok"], true);
    assert_eq!(v["clients"], serde_json::json!(["claude"]));
    assert!(v["actions"].as_array().unwrap().iter().all(|a| a["client"] == "claude"));
}

// ------------------------------------------------------------------------------------ log

#[test]
fn log_validates_kind_and_message_and_records_paths_from_the_root() {
    let (_d, root) = project();
    let o = kb(&root, &["log", "x", "--kind", "bogus"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("Unknown event kind"), "{}", stderr(&o));
    let o = kb(&root, &["log", "   "]);
    assert_eq!(o.status.code(), Some(2));
    let o = kb(&root, &["log", "x", "--kind", "averyveryverylongkind"]);
    assert_eq!(o.status.code(), Some(2));

    let sub = root.join("sub/dir");
    fs::create_dir_all(&sub).unwrap();
    let abs = root.join("src/b.rs");
    let o = kb(&sub, &["log", "Chose X", "--kind", "Decision", "--file", "src/a.rs", "--file", abs.to_str().unwrap()]);
    assert!(o.status.success(), "{}", stderr(&o));

    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    let events = read_events(&config);
    assert_eq!(events.len(), 1, "invalid entries must not be stored");
    let e = &events[0];
    assert_eq!(e.kind, "decision");
    assert_eq!(e.files, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
    assert_eq!(e.cwd.as_deref(), Some("sub/dir"));
}

// ------------------------------------------------------------------------------- timeline

#[test]
fn timeline_files_query_and_kind_validation() {
    let (_d, root) = project();
    assert!(kb(&root, &["log", "one", "--file", "src/a.rs"]).status.success());
    assert!(kb(&root, &["log", "two", "--file", "src/ab.rs"]).status.success());
    assert!(kb(&root, &["log", "three", "--kind", "risk", "--file", "lib/c.rs"]).status.success());

    let summaries = |o: &Output| -> Vec<String> {
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        let mut s: Vec<String> = v["entries"].as_array().unwrap().iter().map(|e| e["summary"].as_str().unwrap().to_string()).collect();
        s.sort();
        s
    };
    // Exact match (src/a.rs does not match src/ab.rs); repeated --file is an OR.
    let o = kb(&root, &["timeline", "--json", "--file", "src/a.rs"]);
    assert_eq!(summaries(&o), vec!["one"]);
    let o = kb(&root, &["timeline", "--json", "--file", "src/a.rs", "--file", "./lib/c.rs"]);
    assert_eq!(summaries(&o), vec!["one", "three"]);
    let o = kb(&root, &["timeline", "--json", "--kind", "RISK"]);
    assert_eq!(summaries(&o), vec!["three"]);

    for args in [
        vec!["timeline", "--query", ""],
        vec!["timeline", "--query", "  "],
        vec!["timeline", "--kind", "bogus"],
        vec!["timeline", "--file", ""],
    ] {
        let o = kb(&root, &args);
        assert_eq!(o.status.code(), Some(2), "{:?}: {}", args, stderr(&o));
    }
    let mut many = vec!["timeline".to_string()];
    for i in 0..17 {
        many.push("--file".into());
        many.push(format!("f{}.rs", i));
    }
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let o = kb(&root, &many);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("16"), "{}", stderr(&o));
}

#[test]
fn timeline_read_and_output_caps() {
    let d = tempdir().unwrap();
    let root = d.path().to_path_buf();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    fs::create_dir_all(config.decisions_log_path().parent().unwrap()).unwrap();

    // 64 KiB output cap: large entries are dropped whole, never shortened.
    let big = "x".repeat(2000);
    let mut log = String::new();
    for i in 0..100 {
        log.push_str(&format!(
            "{{\"id\":\"{i}\",\"timestamp\":\"2026-01-01T00:00:{:02}Z\",\"kind\":\"note\",\"summary\":\"{big}\"}}\n",
            i % 60
        ));
    }
    fs::write(config.decisions_log_path(), &log).unwrap();
    let filter = TimelineFilter { limit: 200, ..Default::default() };
    let r = query_timeline_files(&config, filter.clone(), &[]);
    let size: usize = r.entries.iter().map(|e| serde_json::to_string_pretty(e).unwrap().len()).sum();
    assert!(size <= MAX_TIMELINE_OUTPUT_BYTES, "{}", size);
    assert!(r.truncated && r.entries.len() < 100 && !r.entries.is_empty());
    assert!(r.entries.iter().all(|e| e.summary.len() == 2000));

    // 10,000-entry read cap: only the newest lines are read.
    let mut log = String::new();
    for i in 0..(MAX_TIMELINE_READ_ENTRIES + 50) {
        log.push_str(&format!("{{\"id\":\"{i}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"kind\":\"note\",\"summary\":\"e{i}\"}}\n"));
    }
    fs::write(config.decisions_log_path(), &log).unwrap();
    let r = query_timeline_files(&config, TimelineFilter { query: Some("e5".into()), ..filter }, &[]);
    assert!(r.source_truncated && r.truncated);
    assert!(r.total_matched <= MAX_TIMELINE_READ_ENTRIES);
    // "e5" (line 6) is outside the newest 10,000 lines; "e5" prefixes later ones only.
    assert!(r.entries.iter().all(|e| e.summary != "e5"));
}

// ------------------------------------------------------------------ capabilities / heartbeat

#[test]
fn capabilities_without_repository_reports_git_first_and_null_scaffold_id() {
    let d = tempdir().unwrap();
    let o = kb(d.path(), &["capabilities", "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["repository"]["initializationState"], "not_git_repository");
    assert!(v["repository"]["scaffoldId"].is_null(), "{}", v["repository"]["scaffoldId"]);
    assert!(listing(d.path()).is_empty());

    git_init(d.path());
    let v: Value = serde_json::from_slice(&kb(d.path(), &["capabilities", "--json"]).stdout).unwrap();
    assert_eq!(v["repository"]["initializationState"], "scaffold_missing");
    assert!(v["repository"]["scaffoldId"].is_null());
}

#[test]
fn heartbeat_ok_matches_heartbeat_ok_field() {
    let (_d, root) = project();
    // A stale document: reported, so the heartbeat is not all clear.
    let arch = root.join(".knobyte/context/architecture.md");
    let text = fs::read_to_string(&arch).unwrap();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    fs::write(&arch, text.replace(&format!("last_updated: {}", today), "last_updated: 2020-01-01")).unwrap();
    let o = kb(&root, &["heartbeat", "--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["ok"], v["heartbeatOk"], "{}", v);
    assert_eq!(v["ok"], false);
    // Knobyte-only fields never read as "healthy" next to `ok: false`.
    assert_eq!(v["progressSafe"], true);
    assert!(v.get("healthy").is_none() && v.get("memory_cleanup_status").is_none(), "{}", v);
    assert_eq!(v["maintenanceStatus"], "clear");
    assert!(!v.to_string().contains("\"healthy"), "{}", v);
}

// ------------------------------------------------------------- pipes, args and error output

#[test]
fn closed_stdout_does_not_panic() {
    let (_d, root) = project();
    for args in [vec!["capabilities", "--json"], vec!["member", "add", "alex", "--name", "Alex", "--json"], vec!["commands"]] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_knobyte"))
            .args(&args)
            .current_dir(&root)
            .env("HOME", &root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = stderr(&out);
        assert!(!err.contains("panicked") && !err.contains("Broken pipe"), "{:?}: {}", args, err);
    }
}

#[test]
fn argument_validation_and_plain_errors() {
    let (_d, root) = project();
    let o = kb(&root, &["watch", "--interval", "0"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));

    let o = kb(&root, &["export", "--out", ".knobyte/context/architecture.md"]);
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(err.starts_with("[error] "), "{}", err);
    assert!(!err.contains("Error: \""), "Debug-quoted error: {}", err);

    // --port / --no-open belong to bare `knobyte`.
    let o = kb(&root, &["--port", "4555", "check"]);
    assert_eq!(o.status.code(), Some(2));
    let o = kb(&root, &["--port", "notaport"]);
    assert_eq!(o.status.code(), Some(2));
}

// ------------------------------------------------------------------------------------- TUI

#[test]
fn tui_log_entry_chooses_kind_attaches_file_and_records_actor_like_cli() {
    use knobyte::tui::{submit_log, LogDraft};
    let (_d, root) = project();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    let mut draft = LogDraft::new();
    assert_eq!(draft.kind_name(), "note");
    while draft.kind_name() != "risk" {
        draft.next_kind();
    }
    draft.message = "Flaky upstream".into();
    draft.file = "./src/net.rs".into();
    let e = submit_log(&config, &draft).unwrap();
    assert_eq!(e.kind, "risk");
    assert_eq!(e.files, vec!["src/net.rs".to_string()]);
    assert_eq!(e.actor, knobyte::events::logging_actor(&config));
    assert!(e.cwd.is_some());
    assert!(submit_log(&config, &LogDraft::new()).is_err(), "empty message refused");
}
