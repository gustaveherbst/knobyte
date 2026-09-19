use tempfile::tempdir;
use knobyte::config::KnobyteConfig;
use knobyte::setup::run_setup;
use knobyte::team::inbox::{list_inbox_drafts, publish_inbox_draft, save_inbox_draft, InboxDraft};
use knobyte::team::members::{
    get_current_member, list_members, save_member, select_current_member, GitAlias, Member,
};
use knobyte::team::relay::{
    acknowledge_relay, close_relay, list_relays, publish_relay_draft, save_relay_draft, RelayDraft,
};
use knobyte::team::token::{sign_preview_payload, verify_preview_payload};

#[test]
fn test_team_members_and_selection() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let scaffold = root.join(".knobyte");
    let config = KnobyteConfig::new(root, scaffold);
    run_setup(&config, "code-repo", false).unwrap();

    let member = Member {
        id: "mem_alex".to_string(),
        display_name: "Alex Rivera".to_string(),
        git_aliases: vec![GitAlias {
            name: Some("Alex".to_string()),
            email: Some("alex@example.com".to_string()),
        }],
        status: "active".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };

    save_member(&config, &member).unwrap();

    let members = list_members(&config);
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].id, "mem_alex");

    select_current_member(&config, "mem_alex").unwrap();
    let current = get_current_member(&config).unwrap();
    assert_eq!(current.id, "mem_alex");
}

#[test]
fn test_relay_draft_and_lifecycle() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let scaffold = root.join(".knobyte");
    let config = KnobyteConfig::new(root, scaffold);
    run_setup(&config, "code-repo", false).unwrap();

    let draft = RelayDraft {
        id: "draft_123".to_string(),
        title: "Webhook Retry Refactor".to_string(),
        summary: "Migrated webhooks to exponential backoff".to_string(),
        sender: "mem_alex".to_string(),
        open_to_team: true,
        named_recipients: Vec::new(),
        progress: vec!["Unit tests pass".to_string()],
        blockers: Vec::new(),
        next_actions: vec!["Integration test in staging".to_string()],
        evidence: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    save_relay_draft(&config, &draft).unwrap();

    let relay = publish_relay_draft(&config, "draft_123").unwrap();
    assert_eq!(relay.status, "published");

    let relays = list_relays(&config);
    assert_eq!(relays.len(), 1);

    // Acknowledge (claim)
    let claimed = acknowledge_relay(&config, &relay.id, "mem_sam").unwrap();
    assert_eq!(claimed.status, "acknowledged");
    assert_eq!(claimed.claimant, Some("mem_sam".to_string()));

    // Close
    let closed = close_relay(&config, &relay.id, "mem_sam").unwrap();
    assert_eq!(closed.status, "closed");
}

#[test]
fn test_inbox_lifecycle() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let scaffold = root.join(".knobyte");
    let config = KnobyteConfig::new(root, scaffold);
    run_setup(&config, "code-repo", false).unwrap();

    let draft = InboxDraft {
        id: "draft_auth_fix".to_string(),
        target: "architecture".to_string(),
        title: "Session token expiry clarification".to_string(),
        proposed_content: "Tokens expire after 24h of inactivity.".to_string(),
        reason: "Security audit requirement".to_string(),
        author: "mem_alex".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    save_inbox_draft(&config, &draft).unwrap();
    assert_eq!(list_inbox_drafts(&config).len(), 1);

    let proposal = publish_inbox_draft(&config, "draft_auth_fix").unwrap();
    assert_eq!(proposal.status, "pending");
    assert_eq!(list_inbox_drafts(&config).len(), 0);
}

#[test]
fn test_hmac_preview_signature() {
    let dir = tempdir().unwrap();
    let local_dir = dir.path().join("local");

    let payload = r#"{"command":"member.add","data":{"id":"mem_1"}}"#;
    let sig = sign_preview_payload(&local_dir, payload);

    assert!(verify_preview_payload(&local_dir, payload, &sig));
    assert!(!verify_preview_payload(&local_dir, r#"{"tampered":true}"#, &sig));
}
