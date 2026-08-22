pub mod activity;
pub mod catchup;
pub mod cli;
pub mod contract;
pub mod envelope;
pub mod identity;
pub mod inbox;
pub mod members;
pub mod playbooks;
pub mod refs;
pub mod relay;
pub mod specs;
pub mod store;
pub mod token;
pub mod workflow;
pub mod workstreams;

pub use activity::{list_activity, record_activity, ActivityRecord};
pub use envelope::{ErrorCode, TeamEnvelope, TeamError};
pub use identity::{current_actor_id, resolve_actor, ActorRef, ActorResolution, ActorSource};
pub use inbox::{
    approve_proposal, approve_proposal_with, delete_inbox_draft, get_proposal, list_inbox_drafts,
    list_inbox_proposals, normalize_proposal_target, publish_inbox_draft, reject_proposal,
    save_inbox_draft, save_inbox_draft_with_mode, withdraw_proposal, InboxDraft, InboxProposal,
};
pub use members::{
    clear_current_member, create_member, get_current_member, get_member, list_members,
    save_member, select_current_member, GitAlias, Member,
};
pub use relay::{
    acknowledge_relay, close_relay, delete_relay_draft, get_relay, list_relay_drafts,
    list_relays, observe_repo_state, publish_relay_draft, save_relay_draft, Relay, RelayDraft,
};
pub use specs::{get_spec, list_specs, SpecDetail, SpecItem};
pub use token::{sign_preview_payload, verify_preview_payload};
pub use workflow::{ActorChoice, ApplyResult, PreviewEnvelope, TeamCommand};
pub use workstreams::{get_workstream, list_workstreams, save_workstream, Workstream};

/// Validate an identifier that is used to build a file name inside the scaffold
/// (member ids, draft ids, proposal ids, relay ids, ...).
///
/// Allowed: ASCII letters, digits, `_`, `-` and `.`; must not be empty, must not
/// start with `.` and must not contain `..`. This guarantees the id can never
/// escape its directory when joined as `<dir>/<id>.json`.
pub fn validate_entity_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("Identifier must not be empty".to_string());
    }
    if id.len() > 128 {
        return Err(format!("Identifier '{}' is too long (max 128 characters)", id));
    }
    if id.starts_with('.') || id.contains("..") {
        return Err(format!("Invalid identifier '{}'", id));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(format!(
            "Invalid identifier '{}': only letters, digits, '_', '-' and '.' are allowed",
            id
        ));
    }
    Ok(())
}
