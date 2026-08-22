//! State machines and projections: inbox lifecycle + typed changes, relays,
//! members and actor resolution, workstreams, activity, specs.

use std::fs;

use serde_json::json;
use tempfile::tempdir;

use knobyte::config::KnobyteConfig;
use knobyte::setup::run_setup;
use knobyte::team::activity::{activity_timeline, get_activity, list_activity_page};
use knobyte::team::envelope::ErrorCode;
use knobyte::team::identity::{resolve_actor, ActorRef, ActorSource};
use knobyte::team::inbox::{find_entity, get_proposal, inbox_target, list_inbox_proposals_page};
use knobyte::team::members::{create_member, deactivate_member, get_current_member, list_members_page, reactivate_member, select_current_member, update_member, MemberPatch};
use knobyte::team::relay::list_relays_page;
use knobyte::team::specs::{get_spec, list_specs_page, SpecListFilter};
use knobyte::team::workflow::{execute, run_action, ActorChoice, TeamCommand};
use knobyte::team::workstreams::{get_workstream, list_workstreams_page};

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

fn content_draft(target: &str) -> serde_json::Value {
    json!({ "title": "Limits", "target": target, "content": "100 rps", "rationale": "accuracy" })
}

#[test]
fn inbox_withdraw_self_approval_and_reject_guards() {
    let (_d, config) = project();
    team(&config, &["alex", "sam"]);
    let pid = draft_and_publish(&config, "alex", content_draft("context/a.md"));
    assert_eq!(get_proposal(&config, &pid).unwrap().author, "alex");

    // Only the author may withdraw; the author may not reject.
    assert_eq!(act(&config, "sam", json!({ "kind": "inbox.withdraw", "proposalId": pid })).unwrap_err().code, ErrorCode::Unauthorized);
    assert_eq!(act(&config, "alex", json!({ "kind": "inbox.reject", "proposalId": pid })).unwrap_err().code, ErrorCode::Unauthorized);
    // Self-approval needs the explicit acknowledgement.
    assert_eq!(act(&config, "alex", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap_err().code, ErrorCode::Unauthorized);
    let p = act(&config, "alex", json!({ "kind": "inbox.approve", "proposalId": pid, "selfApprove": true })).unwrap();
    assert_eq!(p["status"], "approved");
    assert_eq!(p["selfApproved"], true);

    let pid2 = draft_and_publish(&config, "alex", content_draft("context/b.md"));
    let w = act(&config, "alex", json!({ "kind": "inbox.withdraw", "proposalId": pid2, "rationale": "dup" })).unwrap();
    assert_eq!(w["status"], "withdrawn");
    // Terminal states cannot transition.
    assert!(act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": pid2 })).is_err());
    assert!(act(&config, "alex", json!({ "kind": "inbox.withdraw", "proposalId": pid2 })).is_err());
}

#[test]
fn inbox_conflict_mark_stale_and_repair() {
    let (_d, config) = project();
    team(&config, &["alex", "sam"]);
    fs::write(config.scaffold_root.join("context/c.md"), "v1\n").unwrap();
    let pid = draft_and_publish(&config, "alex", content_draft("context/c.md"));
    let p = get_proposal(&config, &pid).unwrap();
    assert_eq!(p.target_revisions.len(), 1);
    assert!(p.target_revisions[0].revision.is_some());

    // Not stale while the target is unchanged.
    assert_eq!(act(&config, "sam", json!({ "kind": "inbox.mark-stale", "proposalId": pid, "rationale": "x" })).unwrap_err().code, ErrorCode::ValidationFailed);

    fs::write(config.scaffold_root.join("context/c.md"), "v2\n").unwrap();
    let err = act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap_err();
    assert_eq!(err.code, ErrorCode::RevisionConflict);
    assert_eq!(fs::read_to_string(config.scaffold_root.join("context/c.md")).unwrap(), "v2\n");

    let st = act(&config, "sam", json!({ "kind": "inbox.mark-stale", "proposalId": pid, "rationale": "target moved" })).unwrap();
    assert_eq!(st["status"], "stale");
    // Stale cannot be approved; repair returns it to pending with a fresh revision.
    assert!(act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": pid })).is_err());
    let rep = act(&config, "alex", json!({ "kind": "inbox.repair", "proposalId": pid, "replacement": content_draft("context/c.md") })).unwrap();
    assert_eq!(rep["status"], "pending");
    let ok = act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
    assert_eq!(ok["status"], "approved");
    assert!(fs::read_to_string(config.scaffold_root.join("context/c.md")).unwrap().starts_with("v2\n"));

    let page = list_inbox_proposals_page(&config, &["approved".to_string()], None, None).unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(list_inbox_proposals_page(&config, &["bogus".to_string()], None, None).is_err());
}

#[test]
fn typed_knowledge_and_spec_changes() {
    let (_d, config) = project();
    team(&config, &["alex", "sam"]);
    // knowledge.create
    let pid = draft_and_publish(&config, "alex", json!({
        "change": { "kind": "knowledge.create", "entityKind": "decision", "title": "Use Postgres", "body": "We use Postgres.", "summary": "DB choice", "topics": ["db"] },
        "rationale": "ADR", "evidence": [{ "kind": "commit", "hash": "abc1234" }]
    }));
    let p = get_proposal(&config, &pid).unwrap();
    assert_eq!(p.target, "context/use-postgres.md");
    assert_eq!(p.entity_id.as_deref(), Some("kb_use_postgres"));
    assert_eq!(p.target_revisions[0].revision, None);
    act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
    let e = find_entity(&config, "kb_use_postgres").unwrap();
    assert_eq!(e.entity.entity_type, "decision");
    assert_eq!(e.entity.summary.as_deref(), Some("DB choice"));

    // inbox target + knowledge.update with a pinned revision
    let t = inbox_target(&config, "kb_use_postgres").unwrap();
    assert_eq!(t["target"]["kind"], "decision");
    let rev = t["version"]["contentHash"].clone();
    let pid = draft_and_publish(&config, "alex", json!({
        "change": { "kind": "knowledge.update", "target": { "id": "kb_use_postgres" }, "patch": { "summary": "Database choice" } },
        "rationale": "clarify", "targetRevisions": [{ "path": "context/use-postgres.md", "revision": rev }]
    }));
    act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": pid })).unwrap();
    let e = find_entity(&config, "kb_use_postgres").unwrap();
    assert_eq!(e.entity.summary.as_deref(), Some("Database choice"));
    assert_eq!(e.entity.revision, 2);
    assert!(e.entity.body.contains("We use Postgres."));

    // spec.create with relations validated against target kinds
    let spec = draft_and_publish(&config, "alex", json!({ "change": { "kind": "spec.create", "entityKind": "spec", "title": "Auth", "body": "Auth spec" }, "rationale": "r" }));
    act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": spec })).unwrap();
    // requirement derived_from a decision is refused at publication
    let d = act(&config, "alex", json!({ "kind": "inbox.draft.save", "draft": { "change": { "kind": "spec.create", "entityKind": "requirement", "title": "R1", "body": "b", "relation": { "type": "derived_from", "target": { "id": "kb_use_postgres" } } }, "rationale": "r" } }));
    assert!(d.is_err());
    // invalid relation type for a kind
    assert!(act(&config, "alex", json!({ "kind": "inbox.draft.save", "draft": { "change": { "kind": "spec.create", "entityKind": "constraint", "title": "C", "body": "b", "relation": { "type": "derived_from", "target": { "id": "kb_auth" } } }, "rationale": "r" } })).is_err());
    let req = draft_and_publish(&config, "alex", json!({ "change": { "kind": "spec.create", "entityKind": "requirement", "title": "Tokens expire", "body": "24h", "relation": { "type": "derived_from", "target": { "id": "kb_auth" } } }, "rationale": "r" }));
    act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": req })).unwrap();
    let ac = draft_and_publish(&config, "alex", json!({ "change": { "kind": "spec.create", "entityKind": "acceptance_criterion", "title": "Expiry test", "body": "t", "relation": { "type": "verified_by", "target": { "id": "kb_tokens_expire" } } }, "rationale": "r" }));
    act(&config, "sam", json!({ "kind": "inbox.approve", "proposalId": ac })).unwrap();

    let detail = get_spec(&config, "kb_auth").unwrap();
    assert_eq!(detail.hierarchy.requirements.len(), 1);
    assert_eq!(detail.hierarchy.acceptance_criteria.len(), 1);
    assert_eq!(detail.item.lifecycle_state, "in_flight");
    assert_eq!(detail.item.grounding_health, "unverified");

    let page = list_specs_page(&config, &SpecListFilter { lifecycle: Some("in_flight".into()), ..Default::default() }).unwrap();
    assert!(page.items.iter().any(|s| s.id == "kb_auth"));
    assert!(page.items.iter().all(|s| s.kind == "spec"));
    assert!(list_specs_page(&config, &SpecListFilter { lifecycle: Some("bogus".into()), ..Default::default() }).is_err());
    assert!(list_specs_page(&config, &SpecListFilter { topic: Some("none".into()), ..Default::default() }).unwrap().items.is_empty());
}

#[test]
fn relay_bounds_perspectives_and_paging() {
    let (_d, config) = project();
    team(&config, &["alex", "sam", "pat"]);
    let many: Vec<String> = (0..33).map(|i| format!("m{}", i)).collect();
    assert!(act(&config, "alex", json!({ "kind": "relay.draft.save", "draft": { "summary": "x", "recipients": many } })).is_err());
    // Audience members without recipients cannot publish.
    let d = act(&config, "alex", json!({ "kind": "relay.draft.save", "draft": { "summary": "x", "audience": "members" } })).unwrap();
    assert!(act(&config, "alex", json!({ "kind": "relay.publish", "draftId": d["id"] })).is_err());

    let d = act(&config, "alex", json!({ "kind": "relay.draft.save", "draft": {
        "summary": "Handoff", "recipients": ["sam"], "completed": ["a"], "inProgress": ["b"], "decisions": ["c"],
        "unresolvedQuestions": ["d?"], "changedFiles": ["src/x.rs"], "code": [{ "kind": "symbol", "symbolId": "x::y" }],
        "evidence": [{ "kind": "file", "path": "src/x.rs" }], "workstream": "ws1"
    } })).unwrap();
    let r = act(&config, "alex", json!({ "kind": "relay.publish", "draftId": d["id"] })).unwrap();
    assert_eq!(r["audience"], "members");
    assert_eq!(r["unresolvedQuestions"][0], "d?");
    let rid = r["id"].as_str().unwrap().to_string();

    select_current_member(&config, "sam").unwrap();
    assert_eq!(list_relays_page(&config, Some("mine"), &[], None, None, None).unwrap().items.len(), 1);
    assert_eq!(list_relays_page(&config, Some("sent"), &[], None, None, None).unwrap().items.len(), 0);
    select_current_member(&config, "pat").unwrap();
    assert_eq!(list_relays_page(&config, Some("mine"), &[], None, None, None).unwrap().items.len(), 0);
    assert_eq!(list_relays_page(&config, None, &["published".into()], Some("ws1"), None, None).unwrap().items.len(), 1);
    assert_eq!(list_relays_page(&config, None, &["closed".into()], None, None, None).unwrap().items.len(), 0);

    act(&config, "sam", json!({ "kind": "relay.acknowledge", "relayId": rid })).unwrap();
    // A deactivated claimant blocks closing.
    knobyte::team::members::clear_current_member(&config).unwrap();
    deactivate_member(&config, "sam").unwrap();
    assert_eq!(act(&config, "alex", json!({ "kind": "relay.close", "relayId": rid })).unwrap_err().code, ErrorCode::Unauthorized);
    reactivate_member(&config, "sam").unwrap();
    act(&config, "alex", json!({ "kind": "relay.close", "relayId": rid })).unwrap();

    // Paging with cursors.
    for i in 0..3 {
        let d = act(&config, "alex", json!({ "kind": "relay.draft.save", "draft": { "summary": format!("r{}", i) } })).unwrap();
        act(&config, "alex", json!({ "kind": "relay.publish", "draftId": d["id"] })).unwrap();
    }
    let p1 = list_relays_page(&config, None, &[], None, None, Some(2)).unwrap();
    assert_eq!(p1.items.len(), 2);
    let p2 = list_relays_page(&config, None, &[], None, p1.next_cursor.as_deref(), Some(2)).unwrap();
    assert_eq!(p2.items.len(), 2);
    assert!(p2.next_cursor.is_none());
    // A cursor is bound to its filter.
    assert_eq!(list_relays_page(&config, None, &["published".into()], None, p1.next_cursor.as_deref(), Some(2)).unwrap_err().code, ErrorCode::RevisionConflict);
    assert!(list_relays_page(&config, None, &[], None, None, Some(0)).is_err());
}

#[test]
fn members_update_deactivate_and_resolution_sources() {
    let (_d, config) = project();
    // No git identity in a temp dir without config may still resolve globally; selection wins.
    create_member(&config, "alex", "Alex", Some("alex@example.com"), None).unwrap();
    create_member(&config, "sam", "Sam", None, None).unwrap();
    let m = update_member(&config, "alex", &MemberPatch { display_name: Some("Alexandra".into()), role: Some("lead".into()), ..Default::default() }).unwrap();
    assert_eq!(m.display_name, "Alexandra");
    assert_eq!(m.role.as_deref(), Some("lead"));
    assert!(update_member(&config, "alex", &MemberPatch { display_name: Some("Alexandra".into()), ..Default::default() }).is_err());

    select_current_member(&config, "alex").unwrap();
    let r = resolve_actor(&config);
    assert_eq!(r.source, ActorSource::ConfiguredMember);
    assert_eq!(r.actor.member_id(), Some("alex"));
    // A selected member cannot be deactivated.
    assert!(deactivate_member(&config, "alex").is_err());
    deactivate_member(&config, "sam").unwrap();
    assert!(deactivate_member(&config, "sam").is_err());
    assert_eq!(list_members_page(&config, Some(false), None, None).unwrap().items.len(), 1);
    assert_eq!(list_members_page(&config, Some(true), None, None).unwrap().items.len(), 1);
    assert!(select_current_member(&config, "sam").is_err());
    reactivate_member(&config, "sam").unwrap();

    // A stale selection falls back with a diagnostic.
    fs::write(config.local_dir().join("current_member.json"), r#"{"memberId":"ghost","selectedAt":"x"}"#).unwrap();
    let r = resolve_actor(&config);
    assert!(r.diagnostics.iter().any(|d| d.code == "ACTOR_MEMBER_MISSING"));
    assert_ne!(r.source, ActorSource::ConfiguredMember);
    assert!(!matches!(r.actor, ActorRef::Member { ref member_id, .. } if member_id == "ghost"));
    assert!(get_current_member(&config).map(|m| m.id != "ghost").unwrap_or(true));
}

#[test]
fn workstream_full_model_and_filters() {
    let (_d, config) = project();
    team(&config, &["alex"]);
    let ws = act(&config, "alex", json!({ "kind": "workstream.create", "workstream": {
        "id": "ws1", "title": "Billing", "goal": "Ship billing", "summary": "s", "state": "planned",
        "paths": ["src/billing"], "topics": ["billing"], "nextMilestone": "MVP"
    } })).unwrap();
    assert_eq!(ws["owners"][0], "alex");
    assert_eq!(ws["status"], "planned");
    assert!(act(&config, "alex", json!({ "kind": "workstream.update", "workstreamId": "ws1", "patch": { "state": "archived" } })).is_err());
    assert!(act(&config, "alex", json!({ "kind": "workstream.update", "workstreamId": "ws1", "patch": {} })).is_err());
    act(&config, "alex", json!({ "kind": "workstream.update", "workstreamId": "ws1", "patch": { "state": "blocked", "blockers": ["vendor"] } })).unwrap();
    assert_eq!(get_workstream(&config, "ws1").unwrap().blockers, vec!["vendor".to_string()]);
    act(&config, "alex", json!({ "kind": "workstream.create", "workstream": { "id": "ws2", "title": "Other" } })).unwrap();
    act(&config, "alex", json!({ "kind": "workstream.archive", "workstreamId": "ws1" })).unwrap();
    let w = get_workstream(&config, "ws1").unwrap();
    assert_eq!(w.status, "archived");
    assert!(w.blockers.is_empty());
    assert_eq!(list_workstreams_page(&config, &[], false, None, None).unwrap().items.len(), 1);
    assert_eq!(list_workstreams_page(&config, &[], true, None, None).unwrap().items.len(), 2);
    assert_eq!(list_workstreams_page(&config, &["archived".into()], false, None, None).unwrap().items.len(), 1);
    // Knobyte steps/checkpoints still work on the extended model.
    let ws = knobyte::team::workstreams::update_workstream_step(&config, "ws2", "s1", "done", Some("tests"), None).unwrap();
    assert_eq!(ws.steps.len(), 1);
    assert_eq!(ws.checkpoints.len(), 1);
}

#[test]
fn activity_records_carry_provenance_and_page() {
    let (_d, config) = project();
    team(&config, &["alex"]);
    let rec = act(&config, "alex", json!({ "kind": "activity.record", "activity": { "action": "deploy.done", "summary": "Deployed", "subjects": [{ "kind": "commit", "hash": "abcdef1" }] } })).unwrap();
    let a = get_activity(&config, rec["id"].as_str().unwrap()).unwrap();
    assert_eq!(a.schema_version, 2);
    assert!(a.repo_state.is_some());
    assert!(matches!(a.origin, Some(knobyte::team::activity::ActivityOrigin::Custom)));
    let added = list_activity_page(&config, None, None, None).unwrap();
    assert!(added.items.iter().any(|x| x.action == "member.add" && matches!(x.origin, Some(knobyte::team::activity::ActivityOrigin::Workflow { .. }))));
    assert!(list_activity_page(&config, Some("2999-01-01"), None, None).unwrap().items.is_empty());
    assert!(list_activity_page(&config, Some("nonsense"), None, None).is_err());

    knobyte::events::append_event_with(&config, "Chose X", "decision", &[], &[], None, Some("meeting"), Some("decided")).unwrap();
    let tl = activity_timeline(&config, None, None, None, None).unwrap();
    assert!(tl.items.iter().any(|i| i.source == "log" && i.event.as_ref().unwrap().source.as_deref() == Some("meeting")));
    assert!(tl.items.iter().any(|i| i.source == "activity"));
    assert!(activity_timeline(&config, Some("log"), None, None, None).unwrap().items.iter().all(|i| i.source == "log"));
    assert!(activity_timeline(&config, Some("bogus"), None, None, None).is_err());
}

#[test]
fn request_command_round_trip_uses_service_authority() {
    let (_d, config) = project();
    team(&config, &["alex"]);
    // Caller cannot smuggle an actor into the action.
    let cmd = TeamCommand::new(json!({ "kind": "member.select", "memberId": "alex", "actor": "mallory" }));
    assert_eq!(execute(&config, &cmd, &ActorChoice::resolved()).unwrap_err().code, ErrorCode::InvalidRequest);
}

#[test]
fn spec_grounding_health_reports_changed_like_the_wiki() {
    let (_dir, config) = project();
    let src = config.project_root.join("src");
    fs::create_dir_all(&src).unwrap();
    let code = src.join("tax.rs");
    fs::write(&code, "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n").unwrap();
    let rebuild = || {
        let mut engine = knobyte::graph::GraphEngine::open(&config.graph_db_path()).unwrap();
        engine.rebuild(&config.project_root).unwrap();
    };
    rebuild();
    let specs = config.scaffold_root.join("specs");
    fs::create_dir_all(&specs).unwrap();
    fs::write(
        specs.join("tax.md"),
        "---\nid: kb_tax\ntitle: Tax\ntype: spec\ngrounds_to:\n  - function:src/tax.rs:calculate_tax\n---\n# Tax\n\nTax is 20%.\n",
    )
    .unwrap();
    // No baseline yet: unverified (drift's GROUNDING_UNVERIFIED), not fresh.
    assert_eq!(get_spec(&config, "kb_tax").unwrap().item.grounding_health, "unverified");

    // Baseline the grounding the supported way (`knobyte graph ground --rebaseline`): the
    // baseline is committed into the markdown.
    {
        let engine = knobyte::graph::GraphEngine::open(&config.graph_db_path()).unwrap();
        assert_eq!(engine.ground_docs(&config.project_root, &config.scaffold_root).unwrap(), 1);
    }
    let committed = fs::read_to_string(specs.join("tax.md")).unwrap();
    assert!(committed.contains("body_hash"), "{}", committed);
    assert_eq!(get_spec(&config, "kb_tax").unwrap().item.grounding_health, "fresh");
    {
        // The committed baseline alone decides: the graph.db cache is not needed.
        let conn = rusqlite::Connection::open(config.graph_db_path()).unwrap();
        conn.execute("DELETE FROM _knobyte_grounded_source", []).unwrap();
    }
    assert_eq!(get_spec(&config, "kb_tax").unwrap().item.grounding_health, "fresh");

    // The grounded body changes: the spec reports `changed`, and the filter finds it.
    fs::write(&code, "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.25\n}\n").unwrap();
    rebuild();
    let detail = get_spec(&config, "kb_tax").unwrap();
    assert_eq!(detail.item.grounding_health, "changed");
    assert_eq!(detail.groundings[0].health, "changed");
    let page = list_specs_page(&config, &SpecListFilter { grounding: Some("changed".into()), ..Default::default() }).unwrap();
    assert_eq!(page.items.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["kb_tax"]);
    assert!(list_specs_page(&config, &SpecListFilter { grounding: Some("fresh".into()), ..Default::default() }).unwrap().items.is_empty());

    // The wiki index agrees.
    let mut wiki = knobyte::wiki::WikiIndex::open(&config.wiki_db_path()).unwrap();
    wiki.rebuild(&config.scaffold_root).unwrap();
    assert_eq!(wiki.show("kb_tax").unwrap().unwrap().health.as_deref(), Some("changed"));
}

#[test]
fn grounding_without_baseline_is_unverified_in_wiki_and_spec() {
    let (_dir, config) = project();
    let src = config.project_root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("tax.rs"), "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n").unwrap();
    let mut engine = knobyte::graph::GraphEngine::open(&config.graph_db_path()).unwrap();
    engine.rebuild(&config.project_root).unwrap();
    drop(engine);
    let specs = config.scaffold_root.join("specs");
    fs::create_dir_all(&specs).unwrap();
    fs::write(
        specs.join("tax.md"),
        "---\nid: kb_tax\ntitle: Tax\ntype: spec\ngrounds_to:\n  - function:src/tax.rs:calculate_tax\n---\n# Tax\n\nTax is 20%.\n",
    )
    .unwrap();
    // A read-only drift check records nothing.
    knobyte::drift::checker::run_drift_check(&config);

    let detail = get_spec(&config, "kb_tax").unwrap();
    assert_eq!(detail.item.grounding_health, "unverified");
    assert_eq!(detail.groundings[0].health, "unverified");
    assert!(detail.groundings[0].resolved.is_some());
    let page = list_specs_page(&config, &SpecListFilter { grounding: Some("unverified".into()), ..Default::default() }).unwrap();
    assert_eq!(page.items.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["kb_tax"]);
    assert!(list_specs_page(&config, &SpecListFilter { grounding: Some("fresh".into()), ..Default::default() }).unwrap().items.is_empty());

    let mut wiki = knobyte::wiki::WikiIndex::open(&config.wiki_db_path()).unwrap();
    wiki.rebuild(&config.scaffold_root).unwrap();
    assert_eq!(wiki.show("kb_tax").unwrap().unwrap().health.as_deref(), Some("unverified"));
    let rows = wiki.groundings_for("kb_tax").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.as_deref(), Some("unverified"));
}
