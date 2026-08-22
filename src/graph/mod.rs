pub mod agent;
pub(crate) mod build;
pub mod cli;
pub mod cli_agent;
pub mod chunks;
pub mod corpus;
pub mod engine;
pub mod extractor;
pub mod fingerprint;
pub mod git_state;
pub mod ground;
pub mod grounding;
pub(crate) mod links;
pub mod lock;
pub mod maintenance;
pub mod models;
pub mod protocol;
pub mod publication;
pub mod query_plan;
pub mod read;
pub mod reconcile;
pub(crate) mod resolve;
pub mod scope;
pub mod snapshot;
pub mod schema;
pub mod status;
pub(crate) mod tsconfig;
pub mod ts_compiler;

pub use build::ABORT_AFTER_FILES_ENV;
pub use corpus::{scan_corpus, CorpusPolicy, CorpusScan, CoverageReport, SkippedFile};
pub use engine::{
    scan_indexable_files, BuildSummary, GraphEngine, GroundingHit, ImpactEntry, ImpactOptions,
    ImpactReport, IndexableFile, NodeSource, ReadSnapshot,
};
pub use lock::MaintenanceLock;
pub use maintenance::{
    graph_error, maintenance_error, rebuild_graph, rebuild_graph_with, refresh_graph, refresh_graph_with,
    repair_graph, repair_graph_with, CancelHook, GraphMaintenanceError, MaintenanceOptions,
    PublicationReport, RebuildOutcome, RefreshOutcome, RepairReport, SourceChanges,
};
pub use read::{ReadGate, Unavailable};
pub use status::{inspect_status, GraphHealth};
pub use extractor::is_supported_path;
pub use grounding::{
    kinds_equivalent, parse_grounding_ref, readable_ref, readable_ref_for, resolve_grounding_ref,
    GroundingRef, ParsedRef, RefResolution,
};
pub use models::{Edge, FileRecord, GraphStatus, Node};
