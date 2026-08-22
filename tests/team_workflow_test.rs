use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use tempfile::tempdir;

use knobyte::config::KnobyteConfig;
use knobyte::events::read_events;
use knobyte::heartbeat::{check_heartbeat, run_heartbeat};
use knobyte::setup::{apply_setup, run_setup};
use knobyte::skills::sync_skills;
use knobyte::team::activity::list_activity;
use knobyte::team::inbox::{
    approve_proposal, get_proposal, normalize_proposal_target, publish_inbox_draft,
    reject_proposal, save_inbox_draft, save_inbox_draft_with_mode, InboxDraft,
};
use knobyte::team::members::{create_member, get_current_member, list_members, select_current_member};
use knobyte::team::relay::{
    acknowledge_relay, close_relay, delete_relay_draft, get_relay, publish_relay_draft,
    save_relay_draft, RelayDraft,
};
use knobyte::team::specs::get_spec;

fn setup_project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    (dir, config)
}

fn draft(id: &str, target: &str, content: &str) -> InboxDraft {
    InboxDraft {
        id: id.to_string(),
        target: target.to_string(),
        title: format!("Proposal {}", id),
        proposed_content: content.to_string(),
        reason: "Keep docs accurate".to_string(),
        author: "alex".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

fn relay_draft(id: &str, sender: &str, recipients: &[&str]) -> RelayDraft {
    RelayDraft {
        id: id.to_string(),
        title: "Webhook retries".to_string(),
        summary: "Moved to exponential backoff".to_string(),
        sender: sender.to_string(),
        open_to_team: recipients.is_empty(),
        named_recipients: recipients.iter().map(|s| s.to_string()).collect(),
        progress: vec!["unit tests pass".to_string()],
        blockers: Vec::new(),
        next_actions: vec!["staging test".to_string()],
        evidence: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(status.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&status.stderr));
}

fn init_git_repo(dir: &Path, email: &str) {
    git(dir, &["init", "-q", "-b", "feature/relay-test"]);
    git(dir, &["config", "user.email", email]);
    git(dir, &["config", "user.name", "Test User"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

#[test]
fn approve_creates_target_and_sets_status() {
    let (_d, config) = setup_project();
    create_member(&config, "sam", "Sam Lee", Some("sam@example.com"), Some("reviewer")).unwrap();

    save_inbox_draft(&config, &draft("draft_rl", "context/rate-limit.md", "# Rate limits\n\n100 rps.")).unwrap();
    let prop = publish_inbox_draft(&config, "draft_rl").unwrap();
    assert_eq!(prop.status, "pending");
    assert_eq!(prop.mode, "append");

    let approved = approve_proposal(&config, &prop.id, "sam", Some("LGTM")).unwrap();
    assert_eq!(approved.status, "approved");
    assert_eq!(approved.decision_by.as_deref(), Some("sam"));
    assert_eq!(approved.decision_reason.as_deref(), Some("LGTM"));
    assert!(approved.decided_at.is_some());

    let written = fs::read_to_string(config.scaffold_root.join("context/rate-limit.md")).unwrap();
    assert!(written.contains("100 rps."));

    // Persisted
    assert_eq!(get_proposal(&config, &prop.id).unwrap().status, "approved");

    // Cannot decide twice
    assert!(approve_proposal(&config, &prop.id, "sam", None).is_err());
    assert!(reject_proposal(&config, &prop.id, "sam", None).is_err());

    // Activity + decision event recorded
    let acts = list_activity(&config, 50);
    assert!(acts.iter().any(|a| a.action == "inbox.publish"));
    assert!(acts.iter().any(|a| a.action == "inbox.approve" && a.actor == "sam"));
    let events = read_events(&config);
    assert!(events.iter().any(|e| e.kind == "decision"
        && e.provenance.as_deref() == Some(&format!("inbox:{}", prop.id))));
}

#[test]
fn approve_appends_or_replaces_existing_target() {
    let (_d, config) = setup_project();
    create_member(&config, "sam", "Sam Lee", None, None).unwrap();
    let target = config.scaffold_root.join("context/notes.md");
    fs::write(&target, "original line\n").unwrap();

    save_inbox_draft(&config, &draft("draft_a", "context/notes.md", "appended line")).unwrap();
    let p = publish_inbox_draft(&config, "draft_a").unwrap();
    approve_proposal(&config, &p.id, "sam", None).unwrap();
    let content = fs::read_to_string(&target).unwrap();
    assert!(content.starts_with("original line\n"));
    assert!(content.contains("appended line"));

    save_inbox_draft_with_mode(&config, &draft("draft_r", ".knobyte/context/notes.md", "replaced"), "replace").unwrap();
    let p = publish_inbox_draft(&config, "draft_r").unwrap();
    assert_eq!(p.mode, "replace");
    approve_proposal(&config, &p.id, "sam", None).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "replaced\n");

    assert!(save_inbox_draft_with_mode(&config, &draft("draft_x", "context/x.md", "x"), "merge").is_err());
}

#[test]
fn reject_leaves_target_untouched() {
    let (_d, config) = setup_project();
    create_member(&config, "sam", "Sam Lee", None, None).unwrap();
    save_inbox_draft(&config, &draft("draft_no", "context/nope.md", "nope")).unwrap();
    let p = publish_inbox_draft(&config, "draft_no").unwrap();

    let rejected = reject_proposal(&config, &p.id, "sam", Some("Out of scope")).unwrap();
    assert_eq!(rejected.status, "rejected");
    assert_eq!(rejected.decision_reason.as_deref(), Some("Out of scope"));
    assert!(!config.scaffold_root.join("context/nope.md").exists());
    assert!(approve_proposal(&config, &p.id, "sam", None).is_err());
    assert!(list_activity(&config, 50).iter().any(|a| a.action == "inbox.reject"));
}

#[test]
fn decisions_require_registered_reviewer() {
    let (_d, config) = setup_project();
    save_inbox_draft(&config, &draft("draft_q", "context/q.md", "q")).unwrap();
    let p = publish_inbox_draft(&config, "draft_q").unwrap();
    assert!(approve_proposal(&config, &p.id, "ghost", None).is_err());
    assert_eq!(get_proposal(&config, &p.id).unwrap().status, "pending");
}

#[test]
fn path_escapes_are_refused() {
    let (dir, config) = setup_project();
    create_member(&config, "sam", "Sam Lee", None, None).unwrap();

    for bad in ["../outside.md", "/etc/passwd", "context/../../escape.md", "local/current_member.json", "config.json", "context/.hidden.md"] {
        assert!(normalize_proposal_target(bad).is_err(), "target {} should be refused", bad);
    }
    assert_eq!(normalize_proposal_target("rate-limit").unwrap(), "context/rate-limit.md");
    assert_eq!(normalize_proposal_target(".knobyte/topics/a.md").unwrap(), "topics/a.md");

    // A draft with a malicious target is refused at publication.
    save_inbox_draft(&config, &draft("draft_evil", "../../evil.md", "pwned")).unwrap();
    assert!(publish_inbox_draft(&config, "draft_evil").is_err());
    // A proposal file with a malicious target (e.g. pulled from a remote) is refused on approval.
    save_inbox_draft(&config, &draft("draft_ok", "context/ok.md", "fine")).unwrap();
    let p = publish_inbox_draft(&config, "draft_ok").unwrap();
    let path = config.inbox_dir().join(format!("{}.json", p.id));
    let tampered = fs::read_to_string(&path).unwrap().replace("context/ok.md", "../../evil.md");
    fs::write(&path, tampered).unwrap();
    assert!(approve_proposal(&config, &p.id, "sam", None).is_err());
    assert_eq!(get_proposal(&config, &p.id).unwrap().status, "pending");
    assert!(!dir.path().parent().unwrap().join("evil.md").exists());

    // Ids used as file names are validated.
    assert!(get_proposal(&config, "../config").is_err());
    assert!(save_inbox_draft(&config, &draft("../escape", "context/a.md", "a")).is_err());
    assert!(publish_inbox_draft(&config, "../../x").is_err());
    assert!(save_relay_draft(&config, &relay_draft("../r", "alex", &[])).is_err());
    assert!(delete_relay_draft(&config, "../../x").is_err());
    assert!(get_relay(&config, "../config").is_none());
}

#[test]
fn member_add_and_duplicate_refusal() {
    let (_d, config) = setup_project();
    let m = create_member(&config, "alex", "Alex Rivera", Some("alex@example.com"), Some("backend")).unwrap();
    assert_eq!(m.status, "active");
    assert_eq!(m.email.as_deref(), Some("alex@example.com"));
    assert_eq!(m.role.as_deref(), Some("backend"));
    assert_eq!(list_members(&config).len(), 1);

    assert!(create_member(&config, "alex", "Someone Else", None, None).is_err());
    assert!(create_member(&config, "Bad Id", "X", None, None).is_err());
    assert!(create_member(&config, "../x", "X", None, None).is_err());
    assert!(create_member(&config, "ok", "  ", None, None).is_err());
    assert!(create_member(&config, "ok2", "Ok", Some("not-an-email"), None).is_err());
    assert_eq!(list_members(&config).len(), 1);
    assert!(list_activity(&config, 50).iter().any(|a| a.action == "member.add"));
}

#[test]
fn get_current_member_never_writes() {
    let (_d, config) = setup_project();
    let members_dir = config.members_dir();
    let before: Vec<_> = fs::read_dir(&members_dir).unwrap().collect();
    assert!(before.is_empty());

    // No members, no selection: None and nothing written.
    assert!(get_current_member(&config).is_none());
    assert_eq!(fs::read_dir(&members_dir).unwrap().count(), 0);

    // An active member that does not match git email is not silently picked.
    create_member(&config, "zed", "Zed", Some("zed-unmatched@example.invalid"), None).unwrap();
    assert!(get_current_member(&config).is_none());
    assert_eq!(fs::read_dir(&members_dir).unwrap().count(), 1);

    select_current_member(&config, "zed").unwrap();
    assert_eq!(get_current_member(&config).unwrap().id, "zed");
}

#[test]
fn get_current_member_matches_git_email() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path(), "match-me@example.com");
    let config = KnobyteConfig::new(dir.path().to_path_buf(), dir.path().join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();

    create_member(&config, "other", "Other", Some("other@example.com"), None).unwrap();
    create_member(&config, "me", "Me", Some("Match-Me@example.com"), None).unwrap();
    assert_eq!(get_current_member(&config).unwrap().id, "me");
}

#[test]
fn relay_captures_git_state() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path(), "relay@example.com");
    let head = String::from_utf8(
        Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir.path()).output().unwrap().stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let config = KnobyteConfig::new(dir.path().to_path_buf(), dir.path().join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    create_member(&config, "alex", "Alex", None, None).unwrap();
    save_relay_draft(&config, &relay_draft("draft_git", "alex", &[])).unwrap();
    let relay = publish_relay_draft(&config, "draft_git").unwrap();

    assert_eq!(relay.observed_state.branch.as_deref(), Some("feature/relay-test"));
    assert_eq!(relay.observed_state.head_commit.as_deref(), Some(head.as_str()));
    // The freshly created .knobyte/ scaffold is untracked, so the tree is dirty.
    assert!(relay.observed_state.dirty_tree);
    assert!(list_activity(&config, 50).iter().any(|a| a.action == "relay.publish"));
}

#[test]
fn relay_recipient_enforcement_and_sender_close() {
    let (_d, config) = setup_project();
    for id in ["alex", "sam", "pat"] {
        create_member(&config, id, id, None, None).unwrap();
    }

    save_relay_draft(&config, &relay_draft("draft_named", "alex", &["sam"])).unwrap();
    let relay = publish_relay_draft(&config, "draft_named").unwrap();
    assert!(!relay.open_to_team);
    assert_eq!(relay.sender, "alex");

    // Closing requires a prior acknowledgement.
    assert!(close_relay(&config, &relay.id, "alex").unwrap_err().contains("not been acknowledged"));

    let err = acknowledge_relay(&config, &relay.id, "pat").unwrap_err();
    assert!(err.contains("not a named recipient"));
    let acked = acknowledge_relay(&config, &relay.id, "sam").unwrap();
    assert_eq!(acked.claimant.as_deref(), Some("sam"));
    assert_eq!(acked.acknowledged_by.as_deref(), Some("sam"));

    // Unrelated member cannot close; the sender can.
    assert!(close_relay(&config, &relay.id, "pat").is_err());
    let closed = close_relay(&config, &relay.id, "alex").unwrap();
    assert_eq!(closed.status, "closed");
    assert_eq!(closed.closed_by.as_deref(), Some("alex"));

    // Open relays can be claimed by any active member, then closed by the claimant.
    save_relay_draft(&config, &relay_draft("draft_open", "alex", &[])).unwrap();
    let open = publish_relay_draft(&config, "draft_open").unwrap();
    assert!(open.open_to_team);
    acknowledge_relay(&config, &open.id, "pat").unwrap();
    let closed = close_relay(&config, &open.id, "pat").unwrap();
    assert_eq!(closed.status, "closed");

    let acts = list_activity(&config, 50);
    assert!(acts.iter().any(|a| a.action == "relay.acknowledge"));
    assert!(acts.iter().filter(|a| a.action == "relay.close").count() >= 2);
}

#[test]
fn setup_creates_architecture_and_conventions() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    run_setup(&config, "agent-memory", false).unwrap();

    let arch = fs::read_to_string(config.context_dir().join("architecture.md")).unwrap();
    assert!(arch.contains("id: kb_architecture"));
    let conv = fs::read_to_string(config.context_dir().join("conventions.md")).unwrap();
    assert!(conv.contains("id: kb_conventions"));
    assert!(config.scaffold_root.join("HEARTBEAT.md").exists());

    let cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(config.scaffold_root.join("config.json")).unwrap()).unwrap();
    assert_eq!(cfg["mode"], "agent-memory");
    let reloaded = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    assert_eq!(reloaded.mode, "agent-memory");

    let gi = fs::read_to_string(config.scaffold_root.join(".gitignore")).unwrap();
    for p in ["graph.db*", "wiki.db*", "cozo.db*", "local/"] {
        assert!(gi.lines().any(|l| l.trim() == p), "missing {}", p);
    }
    let root_gi = fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(root_gi.starts_with("target/\n"));
    assert!(root_gi.contains(".knobyte/local/"));

    // Re-running is idempotent.
    let again = apply_setup(&reloaded, "agent-memory", true).unwrap();
    assert!(again.actions.is_empty(), "unexpected actions: {:?}", again.actions);

    assert!(apply_setup(&config, "bogus", true).is_err());
}

#[test]
fn setup_dry_run_writes_nothing_and_lists_everything() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));

    let report = apply_setup(&config, "code-repo", true).unwrap();
    assert!(report.dry_run);
    assert!(!config.scaffold_root.exists());
    assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), "target/\n");

    let paths: Vec<&str> = report.actions.iter().map(|a| a.path.as_str()).collect();
    for expected in [
        "config.json",
        ".gitignore",
        "AGENTS.md",
        "ROUTER.md",
        "context/stack.md",
        "context/architecture.md",
        "context/conventions.md",
    ] {
        assert!(
            paths.iter().any(|p| p.ends_with(&format!(".knobyte/{}", expected))),
            "dry run should list {}",
            expected
        );
    }
    assert!(report.actions.iter().any(|a| a.action == "create_dir" && a.path.ends_with("relays")));
    assert!(report
        .actions
        .iter()
        .any(|a| a.action == "modify_file" && a.path == root.join(".gitignore").to_string_lossy()));

    run_setup(&config, "code-repo", true).unwrap();
    assert!(!config.scaffold_root.exists());
}

#[test]
fn heartbeat_ignores_canonical_files_and_cleans_temp() {
    let (_d, config) = setup_project();
    let old = SystemTime::now() - Duration::from_secs(30 * 86400);
    let set_old = |p: &Path| {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(old).unwrap();
    };

    set_old(&config.scaffold_root.join("config.json"));
    set_old(&config.scaffold_root.join("AGENTS.md"));
    // Staleness reads `last_updated` from frontmatter (file age is only a fallback).
    let arch = config.context_dir().join("architecture.md");
    let content = fs::read_to_string(&arch).unwrap();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    fs::write(&arch, content.replace(&format!("last_updated: {}", today), "last_updated: 2020-01-01")).unwrap();

    let tmp = config.scaffold_root.join("context/write.md.tmp");
    fs::write(&tmp, "partial").unwrap();
    set_old(&tmp);
    let lock = config.local_dir().join("index.lock");
    fs::write(&lock, "").unwrap();
    set_old(&lock);
    let fresh_tmp = config.local_dir().join("fresh.tmp");
    fs::write(&fresh_tmp, "in progress").unwrap();

    let report = check_heartbeat(&config, 14);
    assert!(report.healthy, "stale docs must not make heartbeat unhealthy: {:?}", report);
    assert!(!report.ok && !report.heartbeat_ok, "stale docs are reported, so the heartbeat is not all clear");
    let stale: Vec<&str> = report.stale_files.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(stale, vec!["context/architecture.md"]);
    assert_eq!(report.cleanup_candidates.len(), 2);
    assert!(tmp.exists() && lock.exists());

    let cleaned = run_heartbeat(&config, 14, true);
    assert_eq!(cleaned.cleaned.len(), 2);
    assert!(!tmp.exists() && !lock.exists());
    assert!(fresh_tmp.exists());
    assert!(config.scaffold_root.join("config.json").exists());
    assert!(config.context_dir().join("architecture.md").exists());
}

#[test]
fn skills_sync_validates_tool_and_reports_actions() {
    let (_d, config) = setup_project();
    let err = sync_skills(&config, Some("cursor"), false).unwrap_err();
    assert!(err.contains("claude") && err.contains("codex"));

    let first = sync_skills(&config, Some("claude"), false).unwrap();
    assert!(first.actions.iter().all(|a| a.action == "create" && a.client == "claude"));
    let inbox_skill = fs::read_to_string(config.project_root.join(".claude/skills/knobyte-inbox/SKILL.md")).unwrap();
    assert!(inbox_skill.contains("--reason"));
    // No --tool and no aiTools selected during setup: refuse rather than guess.
    let none = sync_skills(&config, None, true).unwrap_err();
    assert!(none.contains("No supported agent is selected"), "{}", none);
    let second = sync_skills(&config, Some("all"), true).unwrap();
    assert!(second.actions.iter().any(|a| a.client == "claude" && a.action == "unchanged"));
    assert!(second.actions.iter().any(|a| a.client == "codex" && a.action == "create"));
    assert!(!config.project_root.join(".agents").exists());
}

#[test]
fn spec_show_by_id_and_path() {
    let (_d, config) = setup_project();
    fs::write(
        config.specs_dir().join("auth.md"),
        "---\nid: kb_spec_auth\ntitle: Auth Spec\ntype: spec\nstatus: active\n---\n\n# Auth\n\nTokens expire.\n",
    )
    .unwrap();
    let s = get_spec(&config, "kb_spec_auth").unwrap();
    assert_eq!(s.item.title, "Auth Spec");
    assert!(s.body.contains("Tokens expire."));
    assert_eq!(get_spec(&config, "specs/auth.md").unwrap().item.id, "kb_spec_auth");
    assert!(get_spec(&config, "missing").is_err());
}
