//! Transactional safety of team mutations: signed previews, revision
//! expectations, locking, the crash-recovery journal and exact replay.

use std::fs;

use serde_json::json;
use tempfile::tempdir;

use knobyte::config::KnobyteConfig;
use knobyte::setup::run_setup;
use knobyte::team::envelope::ErrorCode;
use knobyte::team::inbox::{get_proposal, publish_inbox_draft, save_inbox_draft, InboxDraft};
use knobyte::team::members::{create_member, get_member, select_current_member};
use knobyte::team::store::{list_journal, TeamLock, JOURNAL_COMPLETE};
use knobyte::team::workflow::{apply, execute, parse_envelope, preview, ActorChoice, TeamCommand};

fn project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    (dir, config)
}

fn pending_proposal(config: &KnobyteConfig, author: &str, target: &str) -> String {
    let id = format!("draft_{}", uuid::Uuid::new_v4().simple());
    save_inbox_draft(
        config,
        &InboxDraft {
            id: id.clone(),
            target: target.to_string(),
            title: "Rate limits".to_string(),
            proposed_content: "100 rps".to_string(),
            reason: "Accuracy".to_string(),
            author: author.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        },
    )
    .unwrap();
    publish_inbox_draft(config, &id).unwrap().id
}

fn approve_cmd(id: &str) -> TeamCommand {
    TeamCommand::new(json!({ "kind": "inbox.approve", "proposalId": id }))
}

#[test]
fn preview_writes_nothing_and_apply_performs_exact_changes() {
    let (_d, config) = project();
    let cmd = TeamCommand::new(json!({ "kind": "member.add", "member": { "id": "alex", "displayName": "Alex" } }));
    let env = preview(&config, &cmd, &ActorChoice::resolved()).unwrap();
    assert!(get_member(&config, "alex").is_none(), "preview must not write");
    assert!(env.preview.changes.iter().any(|c| c.path == "team/members/alex.json" && c.kind == "create"));
    // The completed request carries an expectation for every touched file.
    assert!(env.request.expected_revisions.iter().any(|e| e.path == "team/members/alex.json" && e.revision.is_none()));
    assert!(env.receipt.signature.len() == 64);

    // The envelope survives a JSON round trip (CLI file) and applies.
    let text = serde_json::to_string(&json!({ "schemaVersion": 1, "command": "member.add", "mode": "preview", "ok": true, "data": env, "diagnostics": [], "problem": null })).unwrap();
    let parsed = parse_envelope(&text).unwrap();
    let r = apply(&config, &parsed, &ActorChoice::resolved()).unwrap();
    assert!(r.applied && !r.idempotent_replay);
    assert_eq!(get_member(&config, "alex").unwrap().display_name, "Alex");

    // Exact replay is idempotent; the journal records completion.
    let again = apply(&config, &parsed, &ActorChoice::resolved()).unwrap();
    assert!(again.idempotent_replay);
    assert!(list_journal(&config).iter().any(|j| j.operation_id == env.request.operation_id && j.state == JOURNAL_COMPLETE));
}

#[test]
fn stale_envelope_is_refused_when_target_changed() {
    let (_d, config) = project();
    create_member(&config, "sam", "Sam", None, None).unwrap();
    let pid = pending_proposal(&config, "alex", "context/limits.md");
    let env = preview(&config, &approve_cmd(&pid), &ActorChoice::trusted("sam")).unwrap();

    // Someone edits the proposal target after the preview.
    fs::write(config.scaffold_root.join("context/limits.md"), "edited meanwhile\n").unwrap();
    let err = apply(&config, &env, &ActorChoice::trusted("sam")).unwrap_err();
    assert_eq!(err.code, ErrorCode::RevisionConflict, "{}", err);
    assert_eq!(get_proposal(&config, &pid).unwrap().status, "pending");
}

#[test]
fn concurrent_previews_only_one_applies() {
    let (_d, config) = project();
    create_member(&config, "sam", "Sam", None, None).unwrap();
    create_member(&config, "kim", "Kim", None, None).unwrap();
    let pid = pending_proposal(&config, "alex", "context/limits.md");
    let a = preview(&config, &approve_cmd(&pid), &ActorChoice::trusted("sam")).unwrap();
    let b = preview(&config, &TeamCommand::new(json!({ "kind": "inbox.reject", "proposalId": pid, "rationale": "no" })), &ActorChoice::trusted("kim")).unwrap();
    apply(&config, &a, &ActorChoice::trusted("sam")).unwrap();
    let err = apply(&config, &b, &ActorChoice::trusted("kim")).unwrap_err();
    assert_eq!(err.code, ErrorCode::RevisionConflict);
    assert_eq!(get_proposal(&config, &pid).unwrap().status, "approved");
}

#[test]
fn concurrent_threads_never_double_apply() {
    let (_d, config) = project();
    create_member(&config, "sam", "Sam", None, None).unwrap();
    let pid = pending_proposal(&config, "alex", "context/limits.md");
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let config = config.clone();
            let pid = pid.clone();
            std::thread::spawn(move || execute(&config, &approve_cmd(&pid), &ActorChoice::trusted("sam")).is_ok())
        })
        .collect();
    let ok = handles.into_iter().map(|h| h.join().unwrap()).filter(|x| *x).count();
    assert_eq!(ok, 1, "exactly one approval must win");
    let content = fs::read_to_string(config.scaffold_root.join("context/limits.md")).unwrap();
    assert_eq!(content.matches("100 rps").count(), 1);
}

#[test]
fn tampered_envelope_wrong_actor_and_expired_are_refused() {
    let (_d, config) = project();
    create_member(&config, "sam", "Sam", None, None).unwrap();
    create_member(&config, "kim", "Kim", None, None).unwrap();
    let pid = pending_proposal(&config, "alex", "context/limits.md");
    let env = preview(&config, &approve_cmd(&pid), &ActorChoice::trusted("sam")).unwrap();

    let mut tampered = env.clone();
    tampered.request.action["proposalId"] = json!("prop_other");
    assert_eq!(apply(&config, &tampered, &ActorChoice::trusted("sam")).unwrap_err().code, ErrorCode::Unauthorized);

    let mut forged = env.clone();
    forged.preview.summary = "something else".to_string();
    assert_eq!(apply(&config, &forged, &ActorChoice::trusted("sam")).unwrap_err().code, ErrorCode::Unauthorized);

    // A different actor cannot apply someone else's preview.
    assert_eq!(apply(&config, &env, &ActorChoice::trusted("kim")).unwrap_err().code, ErrorCode::Unauthorized);

    // Expired previews are refused (re-sign an old timestamp with the checkout key).
    let mut old = env.clone();
    old.receipt.authority.occurred_at = (chrono::Utc::now() - chrono::Duration::minutes(31)).to_rfc3339();
    knobyte::team::workflow::sign_envelope(&config, &mut old);
    let err = apply(&config, &old, &ActorChoice::trusted("sam")).unwrap_err();
    assert_eq!(err.code, ErrorCode::RevisionConflict);
    assert!(err.detail.contains("30 minutes"), "{}", err);

    // Untouched envelope still applies.
    apply(&config, &env, &ActorChoice::trusted("sam")).unwrap();
}

#[test]
fn reused_operation_id_with_other_content_conflicts() {
    let (_d, config) = project();
    let mut a = TeamCommand::new(json!({ "kind": "member.add", "member": { "id": "a1", "displayName": "A" } }));
    a.operation_id = "op-fixed".to_string();
    execute(&config, &a, &ActorChoice::resolved()).unwrap();
    let mut b = TeamCommand::new(json!({ "kind": "member.add", "member": { "id": "b1", "displayName": "B" } }));
    b.operation_id = "op-fixed".to_string();
    assert_eq!(execute(&config, &b, &ActorChoice::resolved()).unwrap_err().code, ErrorCode::RevisionConflict);
    assert!(get_member(&config, "b1").is_none());
}

#[test]
fn caller_expectations_must_match() {
    let (_d, config) = project();
    create_member(&config, "alex", "Alex", None, None).unwrap();
    let mut cmd = TeamCommand::new(json!({ "kind": "member.update", "memberId": "alex", "patch": { "displayName": "Alexandra" } }));
    cmd.expected_revisions.push(knobyte::team::workflow::RevisionExpectation {
        path: "team/members/alex.json".to_string(),
        revision: Some("sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string()),
    });
    assert_eq!(preview(&config, &cmd, &ActorChoice::resolved()).unwrap_err().code, ErrorCode::RevisionConflict);
}

#[test]
fn crash_after_intent_is_recovered_by_next_mutation() {
    let (_d, config) = project();
    create_member(&config, "sam", "Sam", None, None).unwrap();
    let pid = pending_proposal(&config, "alex", "context/limits.md");
    let env = preview(&config, &approve_cmd(&pid), &ActorChoice::trusted("sam")).unwrap();

    fs::write(config.local_dir().join("team-failpoint"), "after-intent").unwrap();
    let err = apply(&config, &env, &ActorChoice::trusted("sam")).unwrap_err();
    assert_eq!(err.code, ErrorCode::OperationInterrupted);
    // Intent recorded, nothing written yet.
    assert!(list_journal(&config).iter().any(|j| j.state == "intent"));
    assert_eq!(get_proposal(&config, &pid).unwrap().status, "pending");
    assert!(!config.scaffold_root.join("context/limits.md").exists());

    // Any next mutation rolls the interrupted operation forward first.
    let r = execute(&config, &TeamCommand::new(json!({ "kind": "member.add", "member": { "id": "kim", "displayName": "Kim" } })), &ActorChoice::resolved()).unwrap();
    assert_eq!(r.recovered.len(), 1);
    assert_eq!(r.recovered[0].state, "recovered");
    assert_eq!(get_proposal(&config, &pid).unwrap().status, "approved");
    assert!(fs::read_to_string(config.scaffold_root.join("context/limits.md")).unwrap().contains("100 rps"));
    assert!(knobyte::events::read_events(&config).iter().any(|e| e.provenance.as_deref() == Some(&format!("inbox:{}", pid))));

    // Re-applying the same envelope is an exact replay.
    assert!(apply(&config, &env, &ActorChoice::trusted("sam")).unwrap().idempotent_replay);
}

#[test]
fn recovery_refuses_to_clobber_foreign_edits() {
    let (_d, config) = project();
    create_member(&config, "sam", "Sam", None, None).unwrap();
    let pid = pending_proposal(&config, "alex", "context/limits.md");
    let env = preview(&config, &approve_cmd(&pid), &ActorChoice::trusted("sam")).unwrap();
    fs::write(config.local_dir().join("team-failpoint"), "after-intent").unwrap();
    apply(&config, &env, &ActorChoice::trusted("sam")).unwrap_err();
    fs::write(config.scaffold_root.join("context/limits.md"), "someone else\n").unwrap();
    let reports = knobyte::team::workflow::recover(&config).unwrap();
    assert_eq!(reports[0].state, "conflicted");
    assert_eq!(fs::read_to_string(config.scaffold_root.join("context/limits.md")).unwrap(), "someone else\n");
}

#[test]
fn lock_is_exclusive_and_stale_locks_are_broken() {
    let (_d, config) = project();
    let held = TeamLock::acquire(&config).unwrap();
    assert!(TeamLock::acquire_with_timeout(&config, std::time::Duration::from_millis(100)).is_err());
    drop(held);
    let again = TeamLock::acquire_with_timeout(&config, std::time::Duration::from_millis(100)).unwrap();
    drop(again);
    // Abandoned lock (old mtime) is broken.
    let path = knobyte::team::store::lock_path(&config);
    fs::write(&path, "{\"pid\":1}").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
    assert!(TeamLock::acquire_with_timeout(&config, std::time::Duration::from_millis(100)).is_ok());
}

#[test]
fn atomic_writes_leave_no_temp_files() {
    let (_d, config) = project();
    create_member(&config, "alex", "Alex", None, None).unwrap();
    select_current_member(&config, "alex").unwrap();
    for e in fs::read_dir(config.members_dir()).unwrap().flatten() {
        assert!(!e.file_name().to_string_lossy().ends_with(".tmp"));
    }
    assert!(!knobyte::team::store::lock_path(&config).exists(), "lock released");
}
