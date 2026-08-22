//! Playbooks (definitions and runs) and the Catch Up digest/cursor: workflow
//! semantics, lifecycle guards, CLI envelopes and the preview -> apply round trip.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::tempdir;

use knobyte::config::KnobyteConfig;
use knobyte::setup::run_setup;
use knobyte::team::catchup::{catch_up_digest, get_cursor, CatchUpRequest};
use knobyte::team::playbooks::{get_playbook, get_run, list_runs, playbooks_dir};
use knobyte::team::workflow::{execute, preview, run_action, ActorChoice, TeamCommand};
use knobyte::team::{ErrorCode, TeamError};

fn setup_project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    knobyte::team::members::create_member(&config, "alex", "Alex", None, None).unwrap();
    knobyte::team::members::create_member(&config, "bob", "Bob", None, None).unwrap();
    knobyte::team::members::select_current_member(&config, "alex").unwrap();
    (dir, config)
}

fn act(config: &KnobyteConfig, action: Value) -> Result<Value, TeamError> {
    run_action(config, action, &ActorChoice::resolved()).map(|r| r.result)
}

fn as_bob(config: &KnobyteConfig, action: Value) -> Value {
    run_action(config, action, &ActorChoice::trusted("bob")).unwrap().result
}

fn release_playbook(config: &KnobyteConfig, state: &str) -> Value {
    act(config, json!({ "kind": "playbook.create", "playbook": {
        "title": "Release a version",
        "summary": "Cut and publish a release",
        "state": state,
        "topics": ["release"],
        "steps": [
            { "title": "Run the tests", "description": "cargo test --all", "expectedEvidence": ["test output"] },
            { "title": "Tag the release", "requiredChecks": ["CHANGELOG updated"] }
        ]
    }}))
    .unwrap()
}

#[test]
fn playbook_lifecycle_and_runs() {
    let (_dir, config) = setup_project();

    let pb = release_playbook(&config, "draft");
    assert_eq!(pb["id"], "release-a-version");
    assert_eq!(pb["state"], "draft");
    assert_eq!(pb["owners"], json!(["alex"]));
    assert_eq!(pb["steps"][0]["id"], "run-the-tests");
    assert_eq!(pb["entityRevision"], 1);
    assert!(playbooks_dir(&config).join("release-a-version.json").exists());

    // A draft cannot be run.
    let err = act(&config, json!({ "kind": "playbook.run.start", "playbookId": "release-a-version" })).unwrap_err();
    assert_eq!(err.code, ErrorCode::ValidationFailed);

    // Publishing = update to active; archiving through update is refused.
    let err = act(&config, json!({ "kind": "playbook.update", "playbookId": "release-a-version", "patch": { "state": "archived" } })).unwrap_err();
    assert!(err.detail.contains("playbook archive"), "{}", err.detail);
    let pb = act(&config, json!({ "kind": "playbook.update", "playbookId": "release-a-version", "patch": { "state": "active" } })).unwrap();
    assert_eq!(pb["state"], "active");
    assert_eq!(pb["entityRevision"], 2);

    // Unknown owners and duplicate step ids are refused.
    let err = act(&config, json!({ "kind": "playbook.update", "playbookId": "release-a-version", "patch": { "owners": ["ghost"] } })).unwrap_err();
    assert_eq!(err.code, ErrorCode::ValidationFailed);
    let err = act(&config, json!({ "kind": "playbook.update", "playbookId": "release-a-version", "patch": { "steps": [{ "id": "a", "title": "A" }, { "id": "a", "title": "B" }] } })).unwrap_err();
    assert!(err.detail.contains("Duplicate step id"));

    // Start a run linked to a workstream; steps are snapshotted.
    act(&config, json!({ "kind": "workstream.create", "workstream": { "id": "rel", "title": "Release 1.4" } })).unwrap();
    let run = act(&config, json!({ "kind": "playbook.run.start", "playbookId": "release-a-version", "workstream": "rel", "title": "1.4" })).unwrap();
    let run_id = run["id"].as_str().unwrap().to_string();
    assert_eq!(run["state"], "active");
    assert_eq!(run["workstream"], "rel");
    assert_eq!(run["playbookRevision"], 2);
    assert_eq!(run["steps"].as_array().unwrap().len(), 2);
    assert_eq!(run["steps"][0]["state"], "pending");

    // Later playbook edits do not change the run.
    act(&config, json!({ "kind": "playbook.update", "playbookId": "release-a-version", "patch": { "steps": [{ "title": "Only step" }] } })).unwrap();
    assert_eq!(get_run(&config, &run_id).unwrap().steps.len(), 2);

    // Unknown step, then completing with evidence (warning when evidence expected but missing).
    let err = act(&config, json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": "nope" })).unwrap_err();
    assert_eq!(err.code, ErrorCode::ValidationFailed);
    let env = preview(&config, &TeamCommand::new(json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": "run-the-tests" })), &ActorChoice::resolved()).unwrap();
    assert!(env.preview.diagnostics.iter().any(|d| d.code == "EVIDENCE_MISSING"));
    // Steps are named by id or by 1-based number (`complete-step <run> 1`).
    let err = act(&config, json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": "3" })).unwrap_err();
    assert!(err.detail.contains("out of range"), "{}", err.detail);
    let run = act(&config, json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": "1",
        "evidence": [{ "kind": "manual", "note": "412 passed" }, { "kind": "file", "path": "target/report.txt" }], "note": "green" })).unwrap();
    assert_eq!(run["state"], "active");
    assert_eq!(run["steps"][0]["state"], "completed");
    assert_eq!(run["steps"][0]["completedBy"], "alex");
    assert_eq!(run["steps"][0]["evidence"][0]["note"], "412 passed");

    // Completed steps are immutable; path-escaping evidence is refused.
    let err = act(&config, json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": "run-the-tests" })).unwrap_err();
    assert!(err.detail.contains("already complete"));
    let err = act(&config, json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": "tag-the-release", "evidence": [{ "kind": "file", "path": "../x" }] })).unwrap_err();
    assert_eq!(err.code, ErrorCode::ValidationFailed);

    // Another member completes the last step: the run completes and becomes immutable.
    let run = as_bob(&config, json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": 2 }));
    assert_eq!(run["state"], "completed");
    assert_eq!(run["completedBy"], "bob");
    let err = act(&config, json!({ "kind": "playbook.run.abandon", "runId": run_id, "reason": "late" })).unwrap_err();
    assert!(err.detail.contains("immutable"));

    // Abandon needs a reason.
    let run2 = act(&config, json!({ "kind": "playbook.run.start", "playbookId": "release-a-version" })).unwrap();
    let id2 = run2["id"].as_str().unwrap();
    assert_eq!(run2["steps"].as_array().unwrap().len(), 1);
    assert!(act(&config, json!({ "kind": "playbook.run.abandon", "runId": id2, "reason": " " })).is_err());
    let run2 = act(&config, json!({ "kind": "playbook.run.abandon", "runId": id2, "reason": "superseded" })).unwrap();
    assert_eq!(run2["state"], "abandoned");
    assert_eq!(list_runs(&config).len(), 2);

    // Archive: immutable afterwards, cannot be run.
    let pb = act(&config, json!({ "kind": "playbook.archive", "playbookId": "release-a-version" })).unwrap();
    assert_eq!(pb["state"], "archived");
    assert_eq!(pb["archivedBy"], "alex");
    assert!(act(&config, json!({ "kind": "playbook.update", "playbookId": "release-a-version", "patch": { "title": "X" } })).is_err());
    assert!(act(&config, json!({ "kind": "playbook.archive", "playbookId": "release-a-version" })).is_err());
    assert!(act(&config, json!({ "kind": "playbook.run.start", "playbookId": "release-a-version" })).is_err());

    // Every mutation recorded activity.
    let actions: Vec<String> = knobyte::team::activity::list_activity(&config, 200).into_iter().map(|a| a.action).collect();
    for k in ["playbook.create", "playbook.update", "playbook.archive", "playbook.run.start", "playbook.run.complete-step", "playbook.run.abandon"] {
        assert!(actions.iter().any(|a| a == k), "missing activity {}", k);
    }
}

#[test]
fn playbook_mutations_need_an_active_member() {
    let (_dir, config) = setup_project();
    knobyte::team::members::clear_current_member(&config).unwrap();
    let r = run_action(&config, json!({ "kind": "playbook.create", "playbook": { "title": "X" } }), &ActorChoice::resolved());
    // Without a selected member the actor is a Git identity (or unknown): refused.
    if let Err(e) = r {
        assert_eq!(e.code, ErrorCode::Unauthorized);
    } else {
        // A unique Git alias resolved to a member: acceptable only if that member is active.
        assert!(knobyte::team::identity::resolve_actor(&config).actor.member_id().is_some());
    }
    // A requested actor must be the resolved actor.
    knobyte::team::members::select_current_member(&config, "alex").unwrap();
    let err = run_action(&config, json!({ "kind": "playbook.create", "playbook": { "title": "X" } }), &ActorChoice::member("bob")).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unauthorized);
}

#[test]
fn catch_up_collapses_wiki_migration_operations() {
    let (_dir, config) = setup_project();
    fs::create_dir_all(config.scaffold_root.join("notes")).unwrap();
    fs::write(config.scaffold_root.join("notes/a.md"), "# A\n\nPlain note.\n").unwrap();
    fs::write(config.scaffold_root.join("notes/b.md"), "# B\n\nAnother note.\n").unwrap();
    let scope = knobyte::wiki::scope::WikiScope::load(&config.scaffold_root);
    let result = knobyte::wiki::migrate::migrate(&knobyte::wiki::migrate::MigrationOptions { scope: &scope, graph_db: None }, false);
    assert!(result.applied && result.plan.items.len() == 4, "{:#?}", result.plan.items);

    let v = catch_up_digest(&config, &CatchUpRequest::default()).unwrap().0;
    let knowledge: Vec<&Value> = v["items"].as_array().unwrap().iter().filter(|i| i["group"] == "knowledge").collect();
    assert_eq!(knowledge.len(), 1, "{:#}", v["items"]);
    assert_eq!(knowledge[0]["title"], "wiki migrate: 4 changes");
    let summary = knowledge[0]["summary"].as_str().unwrap();
    assert!(summary.contains("2 entities in 2 files") && summary.contains("2 implicit_id"), "{}", summary);
    assert_eq!(v["groups"]["knowledge"], 1);
}

#[test]
fn catch_up_digest_groups_and_cursor() {
    let (_dir, config) = setup_project();
    let digest = |since: Option<&str>| catch_up_digest(&config, &CatchUpRequest { since: since.map(String::from), ..Default::default() }).unwrap().0;

    // Bob: a handoff addressed to alex, a pending proposal, a workstream and a decision.
    let d = as_bob(&config, json!({ "kind": "relay.draft.save", "draft": { "title": "Parser handoff", "summary": "Parser done", "recipients": ["alex"] } }));
    as_bob(&config, json!({ "kind": "relay.publish", "draftId": d["id"] }));
    let d = as_bob(&config, json!({ "kind": "inbox.draft.save", "draft": { "change": { "kind": "knowledge.create", "entityKind": "convention", "title": "Errors", "body": "Use thiserror" }, "rationale": "consistency" } }));
    as_bob(&config, json!({ "kind": "inbox.publish", "draftId": d["id"] }));
    as_bob(&config, json!({ "kind": "workstream.create", "workstream": { "id": "billing", "title": "Billing" } }));
    knobyte::events::append_logged_event(&config, "Adopt sled", "decision", &[], &[], Some("bob"), None, None).unwrap();
    // Alex's own change is hidden by default.
    act(&config, json!({ "kind": "workstream.create", "workstream": { "id": "mine", "title": "Mine" } })).unwrap();

    let v = digest(None);
    assert_eq!(v["baselineSource"], "default");
    assert_eq!(v["actorId"], "alex");
    let items = v["items"].as_array().unwrap();
    let group_of = |g: &str| items.iter().filter(|i| i["group"] == g).cloned().collect::<Vec<_>>();
    assert_eq!(group_of("handoffs").len(), 1, "{:#}", v);
    assert!(group_of("handoffs")[0]["summary"].as_str().unwrap().contains("awaiting your acknowledgement"));
    assert_eq!(group_of("reviews").len(), 1);
    assert_eq!(group_of("decisions").len(), 1);
    assert!(group_of("workstreams").iter().any(|i| i["title"] == "Billing"));
    assert!(!group_of("workstreams").iter().any(|i| i["title"] == "Mine"));
    assert_eq!(v["needsAttention"], 2);
    // Handoffs come first.
    assert_eq!(items[0]["group"], "handoffs");
    let with_mine = catch_up_digest(&config, &CatchUpRequest { include_mine: true, ..Default::default() }).unwrap().0;
    assert!(with_mine["items"].as_array().unwrap().iter().any(|i| i["title"] == "Mine"));
    assert!(catch_up_digest(&config, &CatchUpRequest { groups: vec!["bogus".into()], ..Default::default() }).is_err());

    // Mark: local only, no activity, no canonical change.
    let activity_before = knobyte::team::activity::list_activity(&config, 500).len();
    let env = preview(&config, &TeamCommand::new(json!({ "kind": "catchup.mark", "at": v["observedAt"] })), &ActorChoice::resolved()).unwrap();
    assert_eq!(env.preview.scope, "local");
    assert!(env.preview.changes.iter().all(|c| c.path.starts_with("local/catch-up/")));
    execute(&config, &env.request, &ActorChoice::resolved()).unwrap();
    assert_eq!(knobyte::team::activity::list_activity(&config, 500).len(), activity_before);
    let actor = knobyte::team::identity::resolve_actor(&config).actor;
    let cursor = get_cursor(&config, &actor).unwrap();
    assert_eq!(cursor.actor_id, "alex");

    // After marking: only still-open items remain, flagged as not new.
    let v = digest(None);
    assert_eq!(v["baselineSource"], "cursor");
    let items = v["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["new"] == false), "{:#}", v);
    assert!(items.iter().all(|i| i["group"] == "handoffs" || i["group"] == "reviews"));
    // --since overrides the cursor.
    assert_eq!(digest(Some("1d"))["baselineSource"], "since");

    // Moving backwards and into the future is refused; reset can go back.
    assert!(act(&config, json!({ "kind": "catchup.mark", "at": "2000-01-01T00:00:00Z" })).is_err());
    assert!(act(&config, json!({ "kind": "catchup.mark", "at": "2999-01-01T00:00:00Z" })).is_err());
    act(&config, json!({ "kind": "catchup.reset", "to": "30d" })).unwrap();
    assert!(digest(None)["items"].as_array().unwrap().iter().any(|i| i["group"] == "decisions"));
    act(&config, json!({ "kind": "catchup.reset", "clear": true })).unwrap();
    assert!(get_cursor(&config, &actor).is_none());
    assert!(act(&config, json!({ "kind": "catchup.reset", "clear": true, "to": "1d" })).is_err());

    // Each member has their own cursor.
    as_bob(&config, json!({ "kind": "catchup.mark" }));
    assert!(get_cursor(&config, &actor).is_none());
    assert!(config.local_dir().join("catch-up/member-bob.json").exists());
}

#[test]
fn catch_up_cursor_belongs_to_its_branch() {
    let (dir, config) = setup_project();
    let root = dir.path();
    let git = |args: &[&str]| {
        let o = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(o.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&o.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.invalid"]);
    git(&["config", "user.name", "T"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    act(&config, json!({ "kind": "catchup.mark" })).unwrap();
    git(&["checkout", "-q", "-b", "feature"]);

    let (_, diags) = catch_up_digest(&config, &CatchUpRequest::default()).unwrap();
    assert!(diags.iter().any(|d| d.code == "CATCH_UP_BRANCH_CHANGED"));
    let err = act(&config, json!({ "kind": "catchup.mark" })).unwrap_err();
    assert_eq!(err.code, ErrorCode::RevisionConflict);
    assert!(err.detail.contains("reset"));
    let c = act(&config, json!({ "kind": "catchup.reset" })).unwrap();
    assert_eq!(c["branch"], "feature");
    act(&config, json!({ "kind": "catchup.mark" })).unwrap();
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn kb(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap()
}

fn json_of(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("not JSON ({}): {}", e, String::from_utf8_lossy(&o.stdout)))
}

#[test]
fn playbook_and_catch_up_cli() {
    let (dir, config) = setup_project();
    let root = dir.path();

    let o = kb(root, &["playbook", "create", "Onboard a service", "--state", "active", "--topic", "ops",
        "--step", "Add config::Write the service config::config file;reviewer sign-off", "--step", "Deploy", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let v = json_of(&o);
    assert_eq!(v["command"], "playbook.create");
    assert_eq!(v["mode"], "apply");
    assert_eq!(v["data"]["result"]["steps"][0]["expectedEvidence"], json!(["config file", "reviewer sign-off"]));
    assert_eq!(v["data"]["result"]["steps"][0]["description"], "Write the service config");

    let o = kb(root, &["playbook", "list", "--json"]);
    assert_eq!(json_of(&o)["data"]["items"].as_array().unwrap().len(), 1);
    let o = kb(root, &["playbook", "list", "--state", "bogus", "--json"]);
    assert_eq!(o.status.code(), Some(2));
    let o = kb(root, &["playbook", "show", "ghost", "--json"]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(json_of(&o)["problem"]["code"], "NOT_FOUND");
    let o = kb(root, &["playbook", "contract", "--action", "playbook.run.complete-step", "--json"]);
    assert!(json_of(&o)["data"]["commands"]["playbook.run.complete-step"]["request"]["$schema"].is_string());
    let o = kb(root, &["catch-up", "contract", "--json"]);
    assert_eq!(json_of(&o)["data"]["commands"]["catchup.mark"]["mutatesCanonical"], false);

    // Preview -> apply round trip for starting a run.
    let o = kb(root, &["playbook", "run", "start", "onboard-a-service", "--preview", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(list_runs(&config).is_empty());
    let file = root.join("preview.json");
    fs::write(&file, &o.stdout).unwrap();
    let o = kb(root, &["playbook", "run", "start", "--apply", file.to_str().unwrap(), "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let run_id = json_of(&o)["data"]["result"]["id"].as_str().unwrap().to_string();
    // Applying with a different command is a usage error.
    let o = kb(root, &["playbook", "run", "abandon", "--apply", file.to_str().unwrap(), "--json"]);
    assert_eq!(o.status.code(), Some(2));

    let o = kb(root, &["playbook", "run", "complete-step", &run_id, "add-config", "--evidence", "file:config/service.toml", "--evidence", "reviewed by bob", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let o = kb(root, &["playbook", "run", "complete-step", &run_id, "add-config", "--json"]);
    assert_eq!(o.status.code(), Some(1));
    let o = kb(root, &["playbook", "run", "complete-step", &run_id, "deploy", "--operation-id", "op-deploy-1", "--json"]);
    assert!(o.status.success());
    let o = kb(root, &["playbook", "run", "list", "--state", "completed", "--json"]);
    assert_eq!(json_of(&o)["data"]["items"][0]["stepsCompleted"], 2);
    let o = kb(root, &["playbook", "run", "show", &run_id]);
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("config/service.toml"), "{}", text);
    assert_eq!(get_run(&config, &run_id).unwrap().state, "completed");

    let o = kb(root, &["playbook", "archive", "onboard-a-service", "--json"]);
    assert!(o.status.success());
    assert_eq!(get_playbook(&config, "onboard-a-service").unwrap().state, "archived");

    // Catch up: digest, mark (preview first), reset.
    let o = kb(root, &["catch-up", "--include-mine", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let v = json_of(&o);
    assert_eq!(v["command"], "catchup.digest");
    assert!(v["data"]["items"].as_array().unwrap().iter().any(|i| i["group"] == "playbooks"));
    let observed = v["data"]["observedAt"].as_str().unwrap().to_string();
    let o = kb(root, &["catch-up", "mark", "--at", &observed, "--preview", "--json"]);
    assert_eq!(json_of(&o)["data"]["preview"]["scope"], "local");
    assert!(!config.local_dir().join("catch-up").join("member-alex.json").exists());
    let o = kb(root, &["catch-up", "mark", "--at", &observed, "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(config.local_dir().join("catch-up").join("member-alex.json").exists());
    let o = kb(root, &["catch-up"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("all caught up"), "{}", String::from_utf8_lossy(&o.stdout));
    let o = kb(root, &["catch-up", "--since", "nonsense", "--json"]);
    assert_eq!(o.status.code(), Some(2));
    let o = kb(root, &["catch-up", "reset", "--clear", "--json"]);
    assert!(o.status.success());
    assert!(!config.local_dir().join("catch-up").join("member-alex.json").exists());
}
