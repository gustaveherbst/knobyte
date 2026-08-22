//! Project wiki: Markdown entities (file-level frontmatter plus inline `kb:entity` sections),
//! a derived SQLite/FTS5 index, validation, typed operations with an audit log, generated
//! views, export, and agent-driven synthesis.

pub mod cli;
pub mod diagnostics;
pub mod envelope;
pub mod export;
pub mod finalize;
pub mod index;
pub mod maintenance;
pub mod markdown;
pub mod migrate;
pub mod models;
pub mod ops;
pub mod parser;
pub mod paths;
pub mod plans;
pub mod positions;
pub mod scope;
pub mod session;
pub mod synthesis;
pub mod trace;
pub mod validate;
pub mod views;
pub mod yaml;

pub use index::WikiIndex;
pub use models::{EntityRelation, WikiDiagnostic, WikiEntity};
