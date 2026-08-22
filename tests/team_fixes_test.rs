//! Regression tests for team workflow fixes: repair cannot bypass review, relay
//! drafts refuse inactive recipients, bounded checkpoint dirty files, approved
//! knowledge carries `last_updated`, member.add attribution and human-mode error codes.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::json;
use tempfile::tempdir;

use knobyte::config::KnobyteConfig;
use knobyte::setup::run_setup;
use knobyte::team::activity::list_activity;
use knobyte::team::envelope::ErrorCode;
use knobyte::team::inbox::get_proposal;
use knobyte::team::members::{create_member, deactivate_member};
use knobyte::team::workflow::{run_action, ActorChoice};
use knobyte::team::workstreams::{get_workstream, MAX_CHECKPOINT_DIRTY_FILES};

fn project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    (dir, config)
}

fn act(config: &KnobyteConfig, who: &str, action: serde_json::Value) -> Result<serde_json::Value, knobyte::team::TeamError> {
    run_action(config, action, &ActorChoice::trusted(who)).map(|r| r.result)
}

fn team(config: &KnobyteConfig, ids: &[&str]) {
    for id in ids {
        create_member(config, id, id, None, None).unwrap();
    }
}

fn draft_and_publish(config: &KnobyteConfig, who: &str, draft: serde_json::Value) -> String {
    let d = act(config, who, json!({ "kind": "inbox.draft.save", "draft": draft })).unwrap();
    let p = act(config, who, json!({ "kind": "inbox.publish", "draftId": d["id"] })).unwrap();
    p["id"].as_str().unwrap().to_string()
}

fn create_entity(config: &KnobyteConfig, author: &str, reviewer: &str) {
    let pid = draft_and_publish(config, author, json!({
        "change": { "kind": "knowledge.create", "entityKind": "decision", "title": "Use Postgres", "body": "v1", "summary": "DB" },
        "rationale": "ADR"
    }));
    act(config, reviewer, json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
}

fn update_draft(body: &str) -> serde_json::Value {
    json!({ "change": { "kind": "knowledge.update", "target": { "id": "kb_use_postgres" }, "patch": { "body": body } }, "rationale": "refresh" })
}

#[test]
fn repairer_counts_as_author_for_review() {
    let (_d, config) = project();
    team(&config, &["alice", "bob", "carol"]);
    create_entity(&config, "alice", "carol");
    let target = config.scaffold_root.join("context/use-postgres.md");

    // alice proposes v2; the target moves underneath; the proposal goes stale.
    let pid = draft_and_publish(&config, "alice", update_draft("v2"));
    let moved = fs::read_to_string(&target).unwrap().replace("v1", "v1 edited");
    fs::write(&target, moved).unwrap();
    act(&config, "carol", json!({ "kind": "inbox.mark-stale", "proposalId": pid, "rationale": "moved" })).unwrap();

    // bob repairs it with his own content ...
    let rep = act(&config, "bob", json!({ "kind": "inbox.repair", "proposalId": pid, "replacement": update_draft("v3 by bob") })).unwrap();
    assert_eq!(rep["status"], "pending");
    assert_eq!(rep["repairedBy"], json!(["bob"]));
    let p = get_proposal(&config, &pid).unwrap();
    assert!(p.is_contributor("bob") && p.is_contributor("alice") && !p.is_contributor("carol"));
    let a = list_activity(&config, 100).into_iter().find(|a| a.action == "inbox.repair").unwrap();
    assert_eq!(a.actor, "bob");
    assert_eq!(a.metadata.as_ref().unwrap()["repairedBy"], json!(["bob"]));

    // ... and can neither approve nor reject his own content without the guard.
    let err = act(&config, "bob", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unauthorized);
    assert!(err.detail.starts_with("SELF_APPROVAL_REQUIRED"), "{}", err.detail);
    assert_eq!(act(&config, "bob", json!({ "kind": "inbox.reject", "proposalId": pid })).unwrap_err().code, ErrorCode::Unauthorized);
    // The original author is still an author; withdrawing stays author-only.
    assert_eq!(act(&config, "alice", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap_err().code, ErrorCode::Unauthorized);
    assert_eq!(act(&config, "bob", json!({ "kind": "inbox.withdraw", "proposalId": pid })).unwrap_err().code, ErrorCode::Unauthorized);

    // An uninvolved teammate reviews it.
    let ok = act(&config, "carol", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
    assert_eq!(ok["status"], "approved");
    assert!(ok.get("selfApproved").is_none());
}

#[test]
fn inactive_member_cannot_repair() {
    let (_d, config) = project();
    team(&config, &["alice", "bob", "carol"]);
    fs::write(config.scaffold_root.join("context/c.md"), "v1\n").unwrap();
    let pid = draft_and_publish(&config, "alice", json!({ "title": "T", "target": "context/c.md", "content": "x", "rationale": "r" }));
    fs::write(config.scaffold_root.join("context/c.md"), "v2\n").unwrap();
    act(&config, "carol", json!({ "kind": "inbox.mark-stale", "proposalId": pid, "rationale": "moved" })).unwrap();
    deactivate_member(&config, "bob").unwrap();
    let replacement = json!({ "title": "T", "target": "context/c.md", "content": "y", "rationale": "r" });
    let err = act(&config, "bob", json!({ "kind": "inbox.repair", "proposalId": pid, "replacement": replacement })).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unauthorized);
    // The author repairing their own proposal is not recorded as a separate repairer.
    let rep = act(&config, "alice", json!({ "kind": "inbox.repair", "proposalId": pid, "replacement": replacement })).unwrap();
    assert!(rep.get("repairedBy").is_none(), "{}", rep);
}

#[test]
fn approved_knowledge_records_last_updated() {
    let (_d, config) = project();
    team(&config, &["alice", "carol"]);
    create_entity(&config, "alice", "carol");
    let target = config.scaffold_root.join("context/use-postgres.md");
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let text = fs::read_to_string(&target).unwrap();
    assert!(text.contains("last_updated") && text.contains(&today), "{}", text);
    let report = knobyte::drift::checker::run_drift_check(&config);
    let missing: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.code == "MISSING_FRONTMATTER_FIELD" && i.file.ends_with("use-postgres.md"))
        .collect();
    assert!(missing.is_empty(), "{:?}", missing);

    // An update bumps it as well (an older value is replaced).
    fs::write(&target, text.replace(&today, "2001-01-01")).unwrap();
    let pid = draft_and_publish(&config, "alice", update_draft("v2"));
    act(&config, "carol", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
    let text = fs::read_to_string(&target).unwrap();
    assert!(text.contains(&today) && !text.contains("2001-01-01"), "{}", text);
}

#[test]
fn free_text_proposal_to_a_new_file_creates_an_entity() {
    let (_d, config) = project();
    team(&config, &["alice", "carol"]);
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let pid = draft_and_publish(&config, "alice", json!({
        "title": "IPC protocol", "target": "context/ipc.md", "mode": "append",
        "content": "# IPC\n\nLength-prefixed JSON over a Unix socket.\n", "rationale": "Document the socket protocol"
    }));
    act(&config, "carol", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
    let text = fs::read_to_string(config.scaffold_root.join("context/ipc.md")).unwrap();
    assert!(text.starts_with("---\nid: kb_ipc\ntitle: IPC protocol\ntype: architecture\nstatus: in_flight\nsummary: Document the socket protocol\nrevision: 1\n"), "{}", text);
    assert!(text.contains(&format!("last_updated: {}", today)) && text.contains("\n# IPC\n\nLength-prefixed JSON"), "{}", text);
    let entity = knobyte::wiki::parser::parse_markdown_entity("context/ipc.md", &text).unwrap();
    assert_eq!((entity.id.as_str(), entity.status.as_str()), ("kb_ipc", "in_flight"));
    let report = knobyte::drift::checker::run_drift_check(&config);
    let ipc: Vec<_> = report.issues.iter().filter(|i| i.file.ends_with("context/ipc.md")).collect();
    assert!(ipc.is_empty(), "{:?}", ipc);

    // An existing file only gets its content and a `last_updated` bump.
    let existing = config.scaffold_root.join("context/notes.md");
    fs::write(&existing, "---\ntitle: Notes\nlast_updated: 2001-01-01\n---\n\n# Notes\n").unwrap();
    let pid = draft_and_publish(&config, "alice", json!({ "title": "More", "target": "context/notes.md", "content": "More.", "rationale": "r" }));
    act(&config, "carol", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
    assert_eq!(
        fs::read_to_string(&existing).unwrap(),
        format!("---\ntitle: Notes\nlast_updated: {}\n---\n\n# Notes\n\nMore.\n", today)
    );
}

#[test]
fn relay_draft_refuses_unknown_or_inactive_recipients() {
    let (_d, config) = project();
    team(&config, &["alex", "sam", "pat"]);
    deactivate_member(&config, "pat").unwrap();
    let err = act(&config, "alex", json!({ "kind": "relay.draft.save", "draft": { "summary": "x", "recipients": ["pat"] } })).unwrap_err();
    assert_eq!(err.code, ErrorCode::ValidationFailed);
    assert!(err.detail.contains("pat"), "{}", err.detail);
    assert!(act(&config, "alex", json!({ "kind": "relay.draft.save", "draft": { "summary": "x", "recipients": ["ghost"] } })).is_err());
    assert!(knobyte::team::relay::list_relay_drafts(&config).is_empty());
    act(&config, "alex", json!({ "kind": "relay.draft.save", "draft": { "summary": "x", "recipients": ["sam"] } })).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&o.stderr));
}

#[test]
fn checkpoint_dirty_files_are_bounded() {
    let (d, config) = project();
    let root = d.path();
    git(root, &["init", "-q"]);
    for i in 0..(MAX_CHECKPOINT_DIRTY_FILES + 25) {
        fs::write(root.join(format!("f{:03}.txt", i)), "x").unwrap();
    }
    team(&config, &["alex"]);
    act(&config, "alex", json!({ "kind": "workstream.create", "workstream": { "id": "ws", "title": "WS" } })).unwrap();
    act(&config, "alex", json!({ "kind": "workstream.step.update", "workstreamId": "ws", "stepId": "s1", "status": "in_progress" })).unwrap();
    let ws = get_workstream(&config, "ws").unwrap();
    let cp = ws.checkpoints.last().unwrap();
    assert_eq!(cp.dirty_files.len(), MAX_CHECKPOINT_DIRTY_FILES);
    assert!(cp.dirty_files_omitted >= 25, "{}", cp.dirty_files_omitted);
}

#[test]
fn member_add_is_attributed_to_the_real_actor() {
    let (_d, config) = project();
    let resolved = ActorChoice::resolved();
    let actor_of = |id: &str| {
        list_activity(&config, 100).into_iter().find(|a| a.action == "member.add" && a.entity_id == id).unwrap().actor
    };
    // Bootstrap: the very first member of an empty team registers themselves.
    run_action(&config, json!({ "kind": "member.add", "member": { "id": "ada", "displayName": "Ada" } }), &resolved).unwrap();
    assert_eq!(actor_of("ada"), "ada");
    // Later additions by a non-member actor are never attributed to the new member.
    run_action(&config, json!({ "kind": "member.add", "member": { "id": "zed", "displayName": "Zed" } }), &resolved).unwrap();
    assert_ne!(actor_of("zed"), "zed");
    // A member adding a teammate is the recorded actor.
    act(&config, "ada", json!({ "kind": "member.add", "member": { "id": "bob", "displayName": "Bob" } })).unwrap();
    assert_eq!(actor_of("bob"), "ada");
}

#[test]
fn human_mode_errors_print_wire_codes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(["member", "show", "ghost"])
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("NOT_FOUND:"), "{}", err);
    assert!(!err.contains("NotFound"), "{}", err);
}
