pub mod checker;
pub mod sync;

pub use checker::{run_drift_check, DriftIssue, DriftReport, GroundingHealth};
pub use sync::{
    apply_grounding_relocations, find_grounding_relocations, plan_sync, sync_groundings,
    RelocationProposal, SyncAction, SyncReport, SyncResult,
};
