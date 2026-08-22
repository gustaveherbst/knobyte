//! Shared drift-check types: claims extracted from scaffold markdown and the issues checkers
//! report about them.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const SEVERITY_ERROR: &str = "error";
pub const SEVERITY_WARNING: &str = "warning";
pub const SEVERITY_INFO: &str = "info";

/// Issue codes emitted by the drift checkers.
pub mod codes {
    pub const STALE_FILE: &str = "STALE_FILE";
    pub const MISSING_PATH: &str = "MISSING_PATH";
    pub const DEAD_COMMAND: &str = "DEAD_COMMAND";
    pub const DEPENDENCY_MISSING: &str = "DEPENDENCY_MISSING";
    pub const VERSION_MISMATCH: &str = "VERSION_MISMATCH";
    pub const CROSS_FILE_CONFLICT: &str = "CROSS_FILE_CONFLICT";
    pub const DEAD_EDGE: &str = "DEAD_EDGE";
    pub const INDEX_MISSING_ENTRY: &str = "INDEX_MISSING_ENTRY";
    pub const INDEX_ORPHAN_ENTRY: &str = "INDEX_ORPHAN_ENTRY";
    pub const UNDOCUMENTED_SCRIPT: &str = "UNDOCUMENTED_SCRIPT";
    pub const TOOL_CONFIG_DRIFT: &str = "TOOL_CONFIG_DRIFT";
    pub const TODO_FIXME: &str = "TODO_FIXME";
    pub const BROKEN_LINK: &str = "BROKEN_LINK";
    pub const MISSING_FRONTMATTER_FIELD: &str = "MISSING_FRONTMATTER_FIELD";
    /// The YAML frontmatter is invalid (e.g. a duplicate key), so all its fields are ignored.
    pub const FRONTMATTER_PARSE_ERROR: &str = "FRONTMATTER_PARSE_ERROR";
    /// The frontmatter opens with `---` but never closes.
    pub const FRONTMATTER_UNTERMINATED: &str = "FRONTMATTER_UNTERMINATED";
    pub const STALE_PATTERN: &str = "STALE_PATTERN";
    pub const SCAFFOLD_ORPHANED: &str = "SCAFFOLD_ORPHANED";
    pub const GROUNDING_GONE: &str = "GROUNDING_GONE";
    pub const GROUNDING_DRIFT: &str = "GROUNDING_DRIFT";
    pub const GROUNDING_AMBIGUOUS: &str = "GROUNDING_AMBIGUOUS";
    pub const GROUNDING_UNVERIFIED: &str = "GROUNDING_UNVERIFIED";
    pub const GROUNDING_MIXED_SHAPE: &str = "GROUNDING_MIXED_SHAPE";
    pub const GROUNDING_MOVED_BY_NEIGHBORS: &str = "GROUNDING_MOVED_BY_NEIGHBORS";
    /// Knobyte-only: a scaffold file could not be read.
    pub const UNREADABLE_FILE: &str = "UNREADABLE_FILE";
    /// Knobyte-only: the scaffold directory does not exist.
    pub const SCAFFOLD_MISSING: &str = "SCAFFOLD_MISSING";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimKind {
    Path,
    Command,
    Dependency,
    Version,
}

/// A factual statement a scaffold file makes about the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub kind: ClaimKind,
    pub value: String,
    /// Source file, relative to the project root.
    pub source: String,
    /// 1-based line in the source file.
    pub line: usize,
    /// Heading the claim sits under.
    pub section: Option<String>,
    /// The claim is described as deleted / absent ("does NOT use X").
    pub negated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DriftIssue {
    pub code: String,
    pub severity: String,
    /// File the issue is about, relative to the project root (e.g. `.knobyte/context/stack.md`).
    pub file: String,
    pub line: Option<usize>,
    pub message: String,
    /// Grounding reference the issue is about, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Relocation / disambiguation candidate for a grounding issue, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// The claim that triggered the issue, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
}

impl DriftIssue {
    pub fn new(
        code: &str,
        severity: &str,
        file: impl Into<String>,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.to_string(),
            severity: severity.to_string(),
            file: file.into(),
            line,
            message: message.into(),
            symbol: None,
            candidate: None,
            claim: None,
        }
    }

    pub fn from_claim(
        code: &str,
        severity: &str,
        claim: &Claim,
        message: impl Into<String>,
    ) -> Self {
        let mut issue = Self::new(
            code,
            severity,
            claim.source.clone(),
            Some(claim.line),
            message,
        );
        issue.claim = Some(claim.clone());
        issue
    }

    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == SEVERITY_ERROR
    }

    pub fn is_grounding(&self) -> bool {
        self.code.starts_with("GROUNDING_")
    }
}

/// Project-relative, forward-slash form of `path` (absolute when outside the project).
pub fn project_relative(project_root: &Path, path: &Path) -> String {
    match path.strip_prefix(project_root) {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}
