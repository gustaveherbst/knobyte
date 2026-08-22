//! Fixture tests for every drift checker, scoring, staleness thresholds, graph-freshness
//! gating and the sync repair brief.

use std::fs;
use std::path::Path;
use std::process::Command;

use knobyte::config::{DriftSettings, KnobyteConfig, StalenessThresholds};
use knobyte::drift::checkers::{
    anchor_link, broken_link, command, cross_file, dependency, edges, frontmatter_completeness,
    grounding_shape, index_sync, path, script_coverage, stale_pattern, staleness, todo_fixme,
    tool_config_sync, CheckContext,
};
use knobyte::drift::{
    build_sync_brief, build_sync_brief_with, compute_score, extract_claims_from_str,
    run_drift_check, run_drift_check_with, DriftCheckOptions, DriftIssue, GraphState,
    SyncBriefOptions,
};
use knobyte::graph::GraphEngine;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn ctx(root: &Path) -> CheckContext {
    CheckContext::new(root, &root.join(".knobyte"))
}

fn codes(issues: &[DriftIssue]) -> Vec<(String, String)> {
    issues
        .iter()
        .map(|i| (i.code.clone(), i.severity.clone()))
        .collect()
}

fn pair(code: &str, sev: &str) -> (String, String) {
    (code.to_string(), sev.to_string())
}

// ── Scoring ─────────────────────────────────────────────────────────────────────────────────

#[test]
fn score_deducts_per_severity_and_skips_neighbor_moves() {
    let i = |code: &str, sev: &str| DriftIssue::new(code, sev, "f", None, "m");
    assert_eq!(compute_score(&[]), 100);
    assert_eq!(
        compute_score(&[
            i("MISSING_PATH", "error"),
            i("TODO_FIXME", "warning"),
            i("GROUNDING_MIXED_SHAPE", "info"),
        ]),
        86
    );
    assert_eq!(
        compute_score(&[i("GROUNDING_MOVED_BY_NEIGHBORS", "info")]),
        100
    );
    let many: Vec<DriftIssue> = (0..15).map(|_| i("BROKEN_LINK", "error")).collect();
    assert_eq!(compute_score(&many), 0);
}

// ── Path / link / command / dependency / cross-file ─────────────────────────────────────────

#[test]
fn missing_path_severity_and_resolution() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/real.rs", "");
    write(root, "deep/a/b/found_me.py", "");
    write(root, "server/src/routes/quiz.ts", "");
    let md = "# Files\n\nSee `src/real.rs`, `src/gone.rs`, `found_me.py`, `routes/quiz.ts`, `src/example_thing.rs`.\n\n# Removed\n\n`src/old.rs` was removed.\n";
    let claims = extract_claims_from_str(md, ".knobyte/context/files.md");
    let issues = path::check_paths(&claims, &ctx(root));
    let got: Vec<(String, String)> = issues
        .iter()
        .map(|i| (i.message.clone(), i.severity.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "Referenced path does not exist: src/gone.rs".into(),
                "error".into()
            ),
            (
                "Referenced path does not exist: src/example_thing.rs".into(),
                "warning".into()
            ),
        ]
    );
    assert_eq!(issues[0].code, "MISSING_PATH");
    assert_eq!(issues[0].line, Some(3));
    assert!(issues[0].claim.is_some());

    // Pattern files only warn.
    let claims = extract_claims_from_str("# X\n\n`src/gone.rs`\n", ".knobyte/patterns/p.md");
    let issues = path::check_paths(&claims, &ctx(root));
    assert_eq!(codes(&issues), vec![pair("MISSING_PATH", "warning")]);

    // Unrooted references (API routes / placeholders) are prose.
    let claims = extract_claims_from_str("# X\n\n`documents/upload`\n", "x.md");
    assert!(path::check_paths(&claims, &ctx(root)).is_empty());
}

#[test]
fn broken_links_skip_comments_fences_and_externals() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".knobyte/context/ok.md", "");
    let md = "[ok](ok.md) [bad](missing.md) [web](https://x.y) [anchor](#top)\n<!-- [c](nope.md) -->\n```\n[f](nope2.md)\n```\n`[code](nope3.md)`\n";
    let file = root.join(".knobyte/context/a.md");
    let issues = broken_link::check_broken_links(md, &file, ".knobyte/context/a.md", &ctx(root));
    assert_eq!(codes(&issues), vec![pair("BROKEN_LINK", "error")]);
    assert_eq!(issues[0].line, Some(1));
    assert!(issues[0].message.ends_with("missing.md"));

    let file = root.join(".knobyte/patterns/p.md");
    let issues = broken_link::check_broken_links(
        "[bad](zzz.md)\n",
        &file,
        ".knobyte/patterns/p.md",
        &ctx(root),
    );
    assert_eq!(codes(&issues), vec![pair("BROKEN_LINK", "warning")]);
}

#[test]
fn dead_commands_against_package_json_and_makefile() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"scripts":{"build":"tsc","test":"vitest"}}"#,
    );
    write(root, "Makefile", "deploy:\n\techo\n");
    let md = "# Setup\n\n`npm run build` `npm run lint` `make deploy` `make release`\n\n```sh\nyarn test\npnpm missing\n```\n";
    let claims = extract_claims_from_str(md, "s.md");
    let issues = command::check_commands(&claims, &ctx(root));
    let msgs: Vec<&str> = issues.iter().map(|i| i.message.as_str()).collect();
    assert_eq!(
        msgs,
        vec![
            "Script \"lint\" not found in package.json scripts",
            "Make target \"release\" not found in Makefile",
            "Script \"missing\" not found in package.json scripts",
        ]
    );
    assert!(issues
        .iter()
        .all(|i| i.code == "DEAD_COMMAND" && i.severity == "error"));
}

#[test]
fn dependencies_and_versions_against_manifests() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"dependencies":{"react":"^18.2.0","express":"4.21.0"}}"#,
    );
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"x\"\n\n[dependencies]\ntokio = { version = \"1.43\" }\nserde_json = \"1.0\"\n",
    );
    write(
        root,
        "pyproject.toml",
        "[project]\ndependencies = [\"sentence-transformers>=2\"]\n",
    );
    let md = "# Key Libraries\n\n- **React 18** — ui\n- **Express 5** — api\n- **Prisma** — orm\n- **Postgres** — db\n- **Frontend** — label\n- **SPA** — concept\n- **tokio** — async\n- **serde-json** — json\n- **sentence_transformers** — embeddings\n";
    let claims = extract_claims_from_str(md, "stack.md");
    let issues = dependency::check_dependencies(&claims, &ctx(root));
    let got: Vec<(String, String)> = issues
        .iter()
        .map(|i| (i.code.clone(), i.message.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "DEPENDENCY_MISSING".into(),
                "Claimed dependency \"Prisma\" not found in any manifest".into()
            ),
            (
                "VERSION_MISMATCH".into(),
                "Claimed \"Express 5\" but manifest has version \"4.21.0\"".into()
            ),
        ]
    );
    assert!(issues.iter().all(|i| i.severity == "warning"));

    // No readable manifest switches the checker off.
    let empty = tempdir().unwrap();
    assert!(dependency::check_dependencies(&claims, &ctx(empty.path())).is_empty());
}

#[test]
fn cross_file_conflicts() {
    let mut claims = extract_claims_from_str(
        "# Stack\n\n- **React 18** — ui\n\n# Run\n\n`npm run dev`\n",
        "a.md",
    );
    claims.extend(extract_claims_from_str(
        "# Stack\n\n- **React 17** — ui\n\n# Run\n\n`yarn dev`\n",
        "b.md",
    ));
    let issues = cross_file::check_cross_file(&claims);
    assert_eq!(
        codes(&issues),
        vec![
            pair("CROSS_FILE_CONFLICT", "error"),
            pair("CROSS_FILE_CONFLICT", "warning")
        ]
    );
    assert!(issues[0].message.contains("a.md:3 says \"React 18\""));
    assert!(issues[1].message.contains("npm, yarn"));
}

// ── Frontmatter-level checkers ──────────────────────────────────────────────────────────────

#[test]
fn dead_edges_and_relations() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".knobyte/context/arch.md", "");
    let fm: serde_json::Value = serde_json::json!({
        "edges": [{"target": "arch.md"}, {"target": "../context/arch.md"}, {"target": "gone.md"}],
        "relations": [{"type": "depends_on", "target_id": "kb_known"}, {"type": "x", "target_id": "kb_unknown"}]
    });
    let ids: std::collections::HashSet<String> = ["kb_known".to_string()].into_iter().collect();
    let file = root.join(".knobyte/context/a.md");
    let issues = edges::check_edges(Some(&fm), &file, ".knobyte/context/a.md", &ctx(root), &ids);
    assert_eq!(
        codes(&issues),
        vec![pair("DEAD_EDGE", "error"), pair("DEAD_EDGE", "error")]
    );
    assert!(issues[0].message.ends_with("gone.md"));
    assert!(issues[1].message.contains("kb_unknown"));
}

#[test]
fn frontmatter_completeness_scope_and_aliases() {
    let fm = serde_json::json!({"title": "T", "summary": "S"});
    let issues = frontmatter_completeness::check_frontmatter_completeness(
        Some(&fm),
        ".knobyte/context/stack.md",
    );
    assert_eq!(
        codes(&issues),
        vec![pair("MISSING_FRONTMATTER_FIELD", "warning")]
    );
    assert!(issues[0].message.contains("last_updated"));

    let none =
        frontmatter_completeness::check_frontmatter_completeness(None, ".knobyte/patterns/x.md");
    assert_eq!(none.len(), 3);
    assert!(frontmatter_completeness::check_frontmatter_completeness(
        None,
        ".knobyte/patterns/INDEX.md"
    )
    .is_empty());
    assert!(
        frontmatter_completeness::check_frontmatter_completeness(None, ".knobyte/ROUTER.md")
            .is_empty()
    );
}

#[test]
fn grounding_shape_mixed_and_conflicting() {
    let mixed = serde_json::json!({"grounds_to": ["function:src/a.rs:f", {"node_id": "function:src/b.rs:g"}]});
    let issues = grounding_shape::check_grounding_shape(Some(&mixed), "x.md");
    assert_eq!(codes(&issues), vec![pair("GROUNDING_MIXED_SHAPE", "info")]);

    let conflict = serde_json::json!({"grounds_to": [{"node_id": "function:src/b.rs:g", "file_path": "src/c.rs"}]});
    let issues = grounding_shape::check_grounding_shape(Some(&conflict), "x.md");
    assert_eq!(
        codes(&issues),
        vec![pair("GROUNDING_MIXED_SHAPE", "warning")]
    );

    let clean = serde_json::json!({"grounds_to": ["function:src/a.rs:f"]});
    assert!(grounding_shape::check_grounding_shape(Some(&clean), "x.md").is_empty());
}

// ── Structural checkers ─────────────────────────────────────────────────────────────────────

#[test]
fn index_sync_missing_and_orphan_entries() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".knobyte/patterns/listed.md", "");
    write(root, ".knobyte/patterns/unlisted.md", "");
    write(
        root,
        ".knobyte/patterns/INDEX.md",
        "- [Listed](listed.md)\n- `ghost.md`\n<!-- [Example](example.md) -->\n",
    );
    let issues = index_sync::check_index_sync(&ctx(root));
    assert_eq!(
        codes(&issues),
        vec![
            pair("INDEX_MISSING_ENTRY", "warning"),
            pair("INDEX_ORPHAN_ENTRY", "warning")
        ]
    );
    assert_eq!(issues[0].file, ".knobyte/patterns/INDEX.md");
    assert!(issues[0].message.contains("patterns/unlisted.md"));
    assert!(issues[1].message.contains("ghost.md"));
}

#[test]
fn stale_patterns_need_an_inbound_reference() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".knobyte/patterns/linked.md", "");
    write(root, ".knobyte/patterns/by_edge.md", "");
    write(root, ".knobyte/patterns/orphan.md", "");
    write(root, ".knobyte/patterns/by_relation.md", "---\nid: kb_pattern_by_relation\n---\n# By relation\n");
    write(
        root,
        ".knobyte/ROUTER.md",
        "---\nrelations:\n  - type: related_to\n    target_id: kb_pattern_by_relation\n---\nSee [linked](patterns/linked.md).\n",
    );
    write(
        root,
        ".knobyte/context/a.md",
        "---\nedges:\n  - target: ../patterns/by_edge.md\n---\nbody\n",
    );
    let issues = stale_pattern::check_stale_patterns(&ctx(root));
    assert_eq!(codes(&issues), vec![pair("STALE_PATTERN", "warning")]);
    assert_eq!(issues[0].file, ".knobyte/patterns/orphan.md");
}

#[test]
fn undocumented_scripts() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"scripts":{"build":"x","prebuild":"x","postinstall":"x","dev":"x","dev:debug":"x","lint":"x"}}"#,
    );
    let issues = script_coverage::check_script_coverage(
        "Run `npm run build` and `npm run dev`.",
        &ctx(root),
    );
    assert_eq!(codes(&issues), vec![pair("UNDOCUMENTED_SCRIPT", "warning")]);
    assert!(issues[0].message.contains("\"lint\""));
    assert_eq!(issues[0].file, "package.json");
}

#[test]
fn todo_fixme_markers_with_lines() {
    let issues = todo_fixme::check_todo_fixme("a\nTODO: x and FIXME\nTODOS are fine\n", "f.md");
    let got: Vec<(Option<usize>, String)> =
        issues.iter().map(|i| (i.line, i.message.clone())).collect();
    assert_eq!(
        got,
        vec![
            (Some(2), "Unresolved TODO marker in scaffold".into()),
            (Some(2), "Unresolved FIXME marker in scaffold".into()),
        ]
    );
}

#[test]
fn tool_config_drift_blames_the_minority() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let base = "<!-- knobyte-tool-config -->\nRead .knobyte/ROUTER.md first.\n";
    write(
        root,
        "CLAUDE.md",
        &format!(
            "{}<!-- knobyte-agent:skills:start -->\nskills\n<!-- knobyte-agent:skills:end -->\n",
            base
        ),
    );
    write(root, ".cursorrules", base);
    write(root, ".windsurfrules", &format!("{}edited\n", base));
    write(root, "AGENTS.md", "hand-written, not a copy\n");
    let issues = tool_config_sync::check_tool_config_sync(&ctx(root));
    assert_eq!(codes(&issues), vec![pair("TOOL_CONFIG_DRIFT", "warning")]);
    assert_eq!(issues[0].file, ".windsurfrules");
    assert!(issues[0].message.contains("CLAUDE.md"));

    fs::remove_file(root.join(".cursorrules")).unwrap();
    let issues = tool_config_sync::check_tool_config_sync(&ctx(root));
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].file, ".windsurfrules");
}

#[test]
fn scaffold_orphaned_when_no_anchor_points_at_it() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".knobyte/ROUTER.md", "# Router\n");
    let issues = anchor_link::check_anchor_link(&ctx(root));
    assert_eq!(codes(&issues), vec![pair("SCAFFOLD_ORPHANED", "error")]);
    assert_eq!(issues[0].file, ".knobyte/ROUTER.md");

    write(root, "CLAUDE.md", "Project notes.\n");
    let issues = anchor_link::check_anchor_link(&ctx(root));
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].file, "CLAUDE.md");

    write(root, "CLAUDE.md", "Read `.knobyte/ROUTER.md` first.\n");
    assert!(anchor_link::check_anchor_link(&ctx(root)).is_empty());

    fs::remove_file(root.join("CLAUDE.md")).unwrap();
    write(root, ".knobyte/config.json", r#"{"ai_tools": []}"#);
    assert!(anchor_link::check_anchor_link(&ctx(root)).is_empty());
}

// ── Staleness ───────────────────────────────────────────────────────────────────────────────

#[test]
fn staleness_threshold_math() {
    let t = StalenessThresholds::default();
    assert!(staleness::staleness_issue("f", Some(29), Some(49), None, &t).is_none());
    let w = staleness::staleness_issue("f", Some(30), None, None, &t).unwrap();
    assert_eq!(
        (w.code.as_str(), w.severity.as_str()),
        ("STALE_FILE", "warning")
    );
    assert_eq!(
        w.message,
        "File hasn't been updated in 30 days (threshold: 30d)"
    );
    // Both signals combine into one issue at the higher severity.
    let e = staleness::staleness_issue("f", Some(40), Some(250), None, &t).unwrap();
    assert_eq!(e.severity, "error");
    assert_eq!(
        e.message,
        "File hasn't been updated in 40 days (threshold: 30d); 250 commits since file was last updated (threshold: 200)"
    );
    let f = staleness::staleness_issue("f", None, None, Some(95), &t).unwrap();
    assert_eq!(f.message, "last_updated is 95 days old (threshold: 90d)");

    let today = chrono::NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();
    assert_eq!(
        staleness::days_since_frontmatter_date(Some("2026-03-01"), today),
        Some(9)
    );
    assert_eq!(
        staleness::days_since_frontmatter_date(Some("[YYYY-MM-DD]"), today),
        None
    );
    assert_eq!(
        staleness::days_since_frontmatter_date(Some("2026-04-01"), today),
        None
    );
    assert_eq!(
        staleness::days_since_frontmatter_date(Some("March 1"), today),
        None
    );
}

fn git(root: &Path, args: &[&str], date: Option<&str>) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    if let Some(d) = date {
        cmd.env("GIT_AUTHOR_DATE", d).env("GIT_COMMITTER_DATE", d);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn staleness_from_git_history_and_cli_thresholds() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"], None);
    write(root, ".knobyte/context/old.md", "old\n");
    git(root, &["add", "-A"], None);
    git(
        root,
        &["commit", "-qm", "old"],
        Some("2000-01-01T00:00:00Z"),
    );
    for i in 0..3 {
        write(root, &format!("src/f{}.rs", i), "");
        git(root, &["add", "-A"], None);
        git(root, &["commit", "-qm", "more"], None);
    }
    assert_eq!(
        staleness::commits_since_last_change(".knobyte/context/old.md", root),
        Some(3)
    );
    assert!(staleness::days_since_last_change(".knobyte/context/old.md", root).unwrap() > 9000);

    let t = StalenessThresholds {
        warn_days: 100_000,
        error_days: 200_000,
        warn_commits: 2,
        error_commits: 10,
    };
    let issues = staleness::check_staleness(".knobyte/context/old.md", root, &t, None);
    assert_eq!(codes(&issues), vec![pair("STALE_FILE", "warning")]);
    assert_eq!(
        issues[0].message,
        "3 commits since file was last updated (threshold: 2)"
    );

    // Untracked files have no git signal.
    assert!(staleness::check_staleness("untracked.md", root, &t, None).is_empty());

    // Thresholds from config.json (camelCase and snake_case both accepted).
    write(
        root,
        ".knobyte/config.json",
        r#"{"stalenessThresholds": {"warnDays": 7, "errorCommits": 3}}"#,
    );
    let s = DriftSettings::load(&root.join(".knobyte")).staleness_thresholds;
    assert_eq!(
        (s.warn_days, s.error_days, s.warn_commits, s.error_commits),
        (7, 90, 50, 3)
    );
    write(
        root,
        ".knobyte/config.json",
        r#"{"staleness_thresholds": {"warn_commits": 1}}"#,
    );
    assert_eq!(
        DriftSettings::load(&root.join(".knobyte"))
            .staleness_thresholds
            .warn_commits,
        1
    );

    // The full check honours explicit thresholds.
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    let report = run_drift_check_with(
        &config,
        &DriftCheckOptions {
            staleness: Some(StalenessThresholds {
                warn_days: 100_000,
                error_days: 200_000,
                warn_commits: 1,
                error_commits: 3,
            }),
            ..Default::default()
        },
    );
    let stale: Vec<&DriftIssue> = report
        .issues
        .iter()
        .filter(|i| i.code == "STALE_FILE")
        .collect();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].severity, "error");
    assert_eq!(stale[0].file, ".knobyte/context/old.md");
}

// ── Full run, grounding, freshness, brief ───────────────────────────────────────────────────

fn project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write(&root, ".knobyte/ROUTER.md", "# Router\n");
    write(&root, "CLAUDE.md", "Read .knobyte/ROUTER.md\n");
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    (dir, config)
}

fn rebuild(config: &KnobyteConfig) {
    let mut engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    engine.rebuild(&config.project_root).unwrap();
}

fn ground(config: &KnobyteConfig) -> usize {
    let engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    engine.ground_all(&config.project_root).unwrap()
}

fn doc(refs: &[&str], anchors: &[&str]) -> String {
    let mut s = String::from("---\ntitle: T\nsummary: S\nlast_updated: 2999-01-01\ngrounds_to:\n");
    for r in refs {
        s.push_str(&format!("  - {}\n", r));
    }
    s.push_str("---\n# Doc\n\nProse.\n\n");
    for a in anchors {
        s.push_str(&format!("<!-- kb-ground: {} -->\n", a));
    }
    s
}

#[test]
fn full_run_reports_every_issue_with_code_severity_file_line() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/real.rs", "pub fn f() {}\n");
    write(
        root,
        ".knobyte/context/a.md",
        "---\ntitle: A\nsummary: S\nlast_updated: 2999-01-01\n---\n# Files\n\nSee `src/real.rs` and `src/gone.rs`. TODO later.\n",
    );
    let report = run_drift_check_with(
        &config,
        &DriftCheckOptions {
            verbose: true,
            ..Default::default()
        },
    );
    assert_eq!(
        codes(&report.issues),
        vec![pair("MISSING_PATH", "error"), pair("TODO_FIXME", "warning")]
    );
    assert_eq!(report.score, 87.0);
    assert_eq!(report.status, "healthy");
    let json = serde_json::to_value(&report).unwrap();
    let first = &json["issues"][0];
    assert_eq!(first["code"], "MISSING_PATH");
    assert_eq!(first["severity"], "error");
    assert_eq!(first["file"], ".knobyte/context/a.md");
    assert_eq!(first["line"], 8);
    let log = report.verbose_log.unwrap();
    // context/a.md, ROUTER.md and the root CLAUDE.md.
    assert_eq!(log[0], "Scaffold files scanned: 3");
    assert!(log.iter().any(|l| l == "Checker paths: 1 issue"));
    // No groundings anywhere and a populated context file: grounding nudge, no score cost.
    assert!(
        report.nudges.iter().any(|n| n.contains("graph")),
        "{:?}",
        report.nudges
    );
}

#[test]
fn grounding_statuses_gone_ambiguous_neighbors_and_drift() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/old.rs", "pub struct Ledger;\nimpl Ledger {\n    pub fn post(&self) -> u32 { 1 }\n}\npub fn twin() -> u32 { 7 }\npub fn calc() -> u32 { 2 }\n");
    rebuild(&config);
    write(
        root,
        ".knobyte/context/g.md",
        &doc(
            &[
                "method:src/old.rs:Ledger::post",
                "function:src/old.rs:twin",
                "function:src/old.rs:calc",
                "function:src/old.rs:never_existed",
            ],
            &["function:src/old.rs:anchor_gone"],
        ),
    );
    // Rebaselining records (and commits) baselines.
    ground(&config);
    let first = run_drift_check(&config);
    assert_eq!(first.grounding.intact, 3, "{:?}", first.issues);
    let gone: Vec<(String, String)> = first
        .issues
        .iter()
        .filter(|i| i.code == "GROUNDING_GONE")
        .map(|i| (i.severity.clone(), i.symbol.clone().unwrap()))
        .collect();
    assert_eq!(
        gone,
        vec![
            ("error".into(), "function:src/old.rs:never_existed".into()),
            ("warning".into(), "function:src/old.rs:anchor_gone".into()),
        ]
    );

    // old.rs disappears: post's identical body exists in two impls (container decides),
    // twin exists identically in two files (ambiguous), calc moved and changed (name match).
    fs::remove_file(root.join("src/old.rs")).unwrap();
    write(root, "src/x.rs", "pub struct Ledger;\nimpl Ledger {\n    pub fn post(&self) -> u32 { 1 }\n}\npub fn twin() -> u32 { 7 }\n");
    write(root, "src/y.rs", "pub struct Other;\nimpl Other {\n    pub fn post(&self) -> u32 { 1 }\n}\npub fn twin() -> u32 { 7 }\npub fn calc() -> u32 { 3 }\n");
    rebuild(&config);

    let report = run_drift_check(&config);
    let by_symbol = |sym: &str| -> Vec<(String, String)> {
        report
            .issues
            .iter()
            .filter(|i| i.symbol.as_deref() == Some(sym))
            .map(|i| (i.code.clone(), i.severity.clone()))
            .collect()
    };
    assert_eq!(
        by_symbol("method:src/old.rs:Ledger::post"),
        vec![pair("GROUNDING_MOVED_BY_NEIGHBORS", "info")]
    );
    let post_notice = report
        .issues
        .iter()
        .find(|i| i.code == "GROUNDING_MOVED_BY_NEIGHBORS")
        .unwrap();
    assert!(
        post_notice.message.contains("matched by its container Ledger, not body"),
        "{}",
        post_notice.message
    );
    // A clean move is not scored, but it is announced.
    assert!(
        report.nudges.iter().any(|n| n.contains("moved")),
        "{:?}",
        report.nudges
    );
    assert_eq!(
        by_symbol("function:src/old.rs:twin"),
        vec![pair("GROUNDING_AMBIGUOUS", "warning")]
    );
    let calc: Vec<&DriftIssue> = report
        .issues
        .iter()
        .filter(|i| i.symbol.as_deref() == Some("function:src/old.rs:calc"))
        .collect();
    assert_eq!(calc.len(), 1);
    assert_eq!(calc[0].code, "GROUNDING_DRIFT");
    assert!(
        calc[0].message.contains("moved and its body changed"),
        "{}",
        calc[0].message
    );
    assert_eq!(calc[0].candidate.as_deref(), Some("function:src/y.rs:calc"));
    assert_eq!(report.grounding.moved, 2);
    assert_eq!(report.grounding.ambiguous, 1);
    assert_eq!(report.grounding.gone, 2);

    // Brief: grouped per file, grounding-aware, with old/new bodies.
    let brief = build_sync_brief(&config, &report);
    assert_eq!(brief.targets.len(), 1);
    assert_eq!(brief.targets[0].file, ".knobyte/context/g.md");
    assert!(brief.prompt.contains("━━━ File 1/1 ━━━"));
    assert!(brief.prompt.contains("GROUNDING REPAIR"));
    assert!(brief
        .prompt
        .contains("Reference: function:src/old.rs:calc (candidate: function:src/y.rs:calc)"));
    assert!(brief.prompt.contains("pub fn calc() -> u32 { 2 }"));
    assert!(brief.prompt.contains("pub fn calc() -> u32 { 3 }"));
    assert_eq!(brief.files.len(), 1);

    // The neighbour-decided move is applied by sync like any other.
    let res = knobyte::drift::sync_groundings(&config, false).unwrap();
    let new_refs: Vec<&str> = res
        .proposals
        .iter()
        .map(|p| p.new_node_id.as_str())
        .collect();
    assert!(
        new_refs.contains(&"method:src/x.rs:Ledger::post"),
        "{:?}",
        new_refs
    );
}

#[test]
fn stale_graph_marks_changed_groundings_unverified() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/a.rs", "pub fn one() -> u32 { 1 }\n");
    write(root, "src/b.rs", "pub fn two() -> u32 { 2 }\n");
    rebuild(&config);
    write(
        root,
        ".knobyte/context/g.md",
        &doc(&["function:src/a.rs:one", "function:src/b.rs:two"], &[]),
    );
    ground(&config);
    assert_eq!(run_drift_check(&config).grounding.intact, 2);

    // Edit b.rs without refreshing the graph: `two` is renamed away, so only a refresh can
    // tell where it went.
    write(root, "src/b.rs", "pub fn deux() -> u32 { 2 }\n");
    let report = run_drift_check(&config);
    let graph = report.graph.as_ref().unwrap();
    assert_eq!(graph.status, GraphState::Stale);
    assert_eq!(graph.modified, vec!["src/b.rs".to_string()]);
    assert_eq!(report.grounding.intact, 1);
    assert_eq!(report.grounding.unverified, 1);
    let g: Vec<&DriftIssue> = report.issues.iter().filter(|i| i.is_grounding()).collect();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].code, "GROUNDING_UNVERIFIED");
    assert!(g[0].message.contains("knobyte graph refresh"));
    assert!(
        report.nudges.iter().any(|n| n.contains("stale")),
        "{:?}",
        report.nudges
    );
}

#[test]
fn missing_graph_skips_grounding_with_a_nudge() {
    let (_d, config) = project();
    write(
        &config.project_root,
        ".knobyte/context/g.md",
        &doc(&["function:src/a.rs:one"], &[]),
    );
    let report = run_drift_check(&config);
    assert_eq!(report.graph.as_ref().unwrap().status, GraphState::Missing);
    assert_eq!(report.grounding.unverified, 1);
    assert!(report.issues.iter().all(|i| !i.is_grounding()));
    assert_eq!(report.score, 100.0);
    assert!(report.nudges[0].contains("Code graph is missing"));
}

#[test]
fn brief_selects_error_files_unless_warnings_requested() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/lib.rs", "pub fn x() {}\n");
    write(
        root,
        ".knobyte/context/err.md",
        "---\ntitle: A\nsummary: S\nlast_updated: 2999-01-01\n---\n# Files\n\n`src/missing.rs` TODO\n",
    );
    write(
        root,
        ".knobyte/context/warn.md",
        "---\ntitle: A\nsummary: S\nlast_updated: 2999-01-01\n---\n# Notes\n\nFIXME later\n",
    );
    let report = run_drift_check(&config);
    let brief = build_sync_brief(&config, &report);
    let files: Vec<&str> = brief.targets.iter().map(|t| t.file.as_str()).collect();
    assert_eq!(files, vec![".knobyte/context/err.md"]);
    assert_eq!(brief.targets[0].issues.len(), 2);
    assert!(brief
        .prompt
        .contains("**Filesystem context (what actually exists):**"));
    assert!(brief.prompt.contains("`src/` contains: lib.rs"));
    assert!(!brief.prompt.contains("GROUNDING REPAIR"));

    let all = build_sync_brief_with(
        &config,
        &report,
        SyncBriefOptions {
            include_warnings: true,
        },
    );
    assert_eq!(all.targets.len(), 2);

    let plan = knobyte::drift::plan_sync(&config, false);
    assert_eq!(plan.actions.len(), 1);
    assert!(plan.actions[0].recommendation.contains("1 error(s)"));
}

#[test]
fn invalid_frontmatter_is_reported_with_its_line_not_as_missing_fields() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".knobyte/ROUTER.md", "# Router\n");
    write(
        root,
        ".knobyte/context/ipc.md",
        "---\nname: IPC\ndescription: Socket protocol\nlast_updated: 2026-01-01\ngrounds_to:\n  - function:Sources/a.swift:run\ngrounds_to:\n  - function:Sources/b.swift:run\n---\n\n# IPC\n",
    );
    write(root, ".knobyte/context/open.md", "---\nname: Open\n\n# never closed\n");
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    let report = run_drift_check(&config);
    let ipc: Vec<&DriftIssue> = report.issues.iter().filter(|i| i.file == ".knobyte/context/ipc.md").collect();
    let parse = ipc.iter().find(|i| i.code == "FRONTMATTER_PARSE_ERROR").expect("parse error reported");
    assert_eq!(parse.severity, "error");
    assert_eq!(parse.line, Some(7), "points at the duplicate key: {}", parse.message);
    assert!(parse.message.contains("duplicate"), "{}", parse.message);
    assert!(!ipc.iter().any(|i| i.code == "MISSING_FRONTMATTER_FIELD"), "{:?}", ipc);
    assert!(report
        .issues
        .iter()
        .any(|i| i.file == ".knobyte/context/open.md" && i.code == "FRONTMATTER_UNTERMINATED" && i.line == Some(1)));
}
