pub mod brief;
pub mod checker;
pub mod checkers;
pub mod claims;
pub mod freshness;
pub mod grounding;
pub mod markdown;
pub mod scoring;
pub mod sync;
pub mod types;

pub use brief::{
    build_sync_brief, build_sync_brief_with, group_into_targets, select_sync_issues, FileBrief,
    SyncBrief, SyncBriefOptions, SyncTarget,
};
pub use checker::{
    grounding_score, run_drift_check, run_drift_check_with, DriftCheckOptions, DriftReport,
    GroundingHealth,
};
pub use claims::{extract_claims, extract_claims_from_str};
pub use freshness::{inspect_graph, GraphFreshness, GraphState};
pub use scoring::compute_score;
pub use sync::{
    apply_grounding_relocations, find_grounding_relocations, plan_sync, reconcile_missing_ref,
    rewrite_grounding_anchors, sync_groundings, MoveEvidence, Reconciliation, RelocationProposal,
    SyncAction, SyncReport, SyncResult,
};
pub use types::{codes, Claim, ClaimKind, DriftIssue};
