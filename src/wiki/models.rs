//! The wiki entity model.
//!
//! Entities are Markdown: a file-level entity keeps its metadata in the file's top-level YAML
//! frontmatter (`id`, `title`, `type`, `status`, `relations`, `grounds_to`, ...), and any number
//! of section entities are declared inline with a `<!-- kb:entity ... -->` marker bound to the
//! heading that follows it. Everything here is the parsed, normalized view of that Markdown.

use serde::{Deserialize, Serialize};

/// Entity types the wiki authors (operations and synthesis may create these).
pub const WIKI_ENTITY_TYPES: [&str; 14] = [
    "architecture",
    "component",
    "decision",
    "convention",
    "pattern",
    "guide",
    "risk",
    "fact",
    "task",
    "topic",
    "spec",
    "requirement",
    "constraint",
    "acceptance_criterion",
];

/// Team-owned entity types the wiki reads but never authors.
pub const TEAM_READABLE_ENTITY_TYPES: [&str; 7] = [
    "member",
    "workstream",
    "proposal",
    "relay",
    "activity",
    "playbook",
    "playbook_run",
];

/// Types older Knobyte scaffolds produced by path inference; accepted for back-compat.
pub const LEGACY_ENTITY_TYPES: [&str; 1] = ["document"];

/// Lifecycle states. Governance, never grounding health.
pub const LIFECYCLE_STATES: [&str; 4] = ["in_flight", "promoted", "deprecated", "archived"];

/// Lifecycle states included in default retrieval.
pub const ACTIVE_LIFECYCLE_STATES: [&str; 2] = ["in_flight", "promoted"];

/// Typed relation vocabulary.
pub const RELATION_TYPES: [&str; 12] = [
    "depends_on",
    "implements",
    "supersedes",
    "contradicts",
    "derived_from",
    "grounded_in",
    "related_to",
    "affects",
    "verified_by",
    "refines",
    "constrained_by",
    "caused_by",
];

/// Evidence kinds for `sources`.
pub const SOURCE_TYPES: [&str; 10] = [
    "file",
    "symbol",
    "commit",
    "pull_request",
    "issue",
    "document",
    "manual",
    "agent_session",
    "test",
    "url",
];

/// Grounding health values, best first.
pub const HEALTH_STATES: [&str; 5] = ["fresh", "unverified", "ambiguous", "changed", "missing"];

/// Spec-driven-development chain: `(from type, to type, relation)` hops.
pub const SDD_CHAIN: [(&str, &str, &str); 3] = [
    ("requirement", "spec", "derived_from"),
    ("decision", "requirement", "implements"),
    ("component", "decision", "implements"),
];

/// Relation from a chain entity to an acceptance criterion.
pub const ACCEPTANCE_CRITERION_RELATION: &str = "verified_by";
/// Relation from a chain entity to a constraint.
pub const CONSTRAINT_RELATION: &str = "constrained_by";
/// Relation a topic uses to name its parent topic (`parent:` in frontmatter is shorthand).
pub const PARENT_TOPIC_RELATION: &str = "depends_on";

pub fn is_lifecycle_state(s: &str) -> bool {
    LIFECYCLE_STATES.contains(&s)
}

pub fn is_active_status(s: &str) -> bool {
    ACTIVE_LIFECYCLE_STATES.contains(&s)
}

pub fn is_relation_type(s: &str) -> bool {
    RELATION_TYPES.contains(&s)
}

pub fn is_source_type(s: &str) -> bool {
    SOURCE_TYPES.contains(&s)
}

/// Map a legacy/free-form status onto a lifecycle state. Returns `(state, recognized_as_legacy)`;
/// `None` when the value is not recognizable at all.
pub fn normalize_lifecycle(raw: &str) -> Option<(&'static str, bool)> {
    let v = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    for s in LIFECYCLE_STATES {
        if v == s {
            return Some((s, false));
        }
    }
    let mapped = match v.as_str() {
        "accepted" | "active" | "approved" | "current" | "stable" | "published" | "done"
        | "final" | "adopted" | "implemented" => "promoted",
        "draft" | "proposed" | "wip" | "in_progress" | "inflight" | "pending" | "open"
        | "review" | "in_review" => "in_flight",
        "superseded" | "obsolete" | "replaced" | "rejected" => "deprecated",
        "retired" | "closed" => "archived",
        _ => return None,
    };
    Some((mapped, true))
}

/// The set of entity types this scaffold accepts. Unregistered types are a diagnostic, not a
/// parse failure.
#[derive(Debug, Clone)]
pub struct EntityTypeRegistry {
    types: Vec<String>,
}

impl Default for EntityTypeRegistry {
    fn default() -> Self {
        Self::with_additional(&[])
    }
}

impl EntityTypeRegistry {
    pub fn with_additional(extra: &[String]) -> Self {
        let mut types: Vec<String> = WIKI_ENTITY_TYPES
            .iter()
            .chain(TEAM_READABLE_ENTITY_TYPES.iter())
            .chain(LEGACY_ENTITY_TYPES.iter())
            .map(|s| s.to_string())
            .collect();
        for t in extra {
            let t = t.trim();
            if !t.is_empty() && !types.iter().any(|x| x == t) {
                types.push(t.to_string());
            }
        }
        types.sort();
        Self { types }
    }

    pub fn has(&self, t: &str) -> bool {
        self.types.iter().any(|x| x == t)
    }

    pub fn list(&self) -> &[String] {
        &self.types
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct EntityRelation {
    #[serde(rename = "type")]
    pub rel_type: String,
    #[serde(alias = "target")]
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Explicit contradiction waiver (`waived: true` on the relation).
    #[serde(default, skip_serializing_if = "is_false")]
    pub waived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Frontmatter key the relation was declared under when it came from a shorthand key such
    /// as `implements: [kb_x]` (None for the `relations:` list). Not serialized.
    #[serde(skip)]
    pub origin: Option<String>,
}

impl EntityRelation {
    pub fn new(rel_type: &str, target: &str) -> Self {
        Self {
            rel_type: rel_type.to_string(),
            target_id: target.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntityGrounding {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

/// Evidence supporting an entity (distinct from provenance, which records who produced it).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WikiSource {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, alias = "capturedAt", skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl WikiSource {
    /// Normalized identity used for deduplication (`type|repository|referent`).
    pub fn identity(&self) -> String {
        let referent = match self.source_type.as_str() {
            "commit" => {
                let sha = self
                    .commit
                    .as_deref()
                    .or(self.reference.as_deref())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                sha.chars().take(7).collect()
            }
            "manual" => self.note.as_deref().unwrap_or("").trim().to_lowercase(),
            "url" => {
                let r = self.reference.as_deref().unwrap_or("").trim();
                match r.split_once("://") {
                    Some((scheme, rest)) => {
                        let (host, path) = match rest.find('/') {
                            Some(i) => (&rest[..i], &rest[i..]),
                            None => (rest, ""),
                        };
                        let path = path.split('#').next().unwrap_or("");
                        format!(
                            "{}://{}{}",
                            scheme.to_ascii_lowercase(),
                            host.to_ascii_lowercase(),
                            path
                        )
                    }
                    None => r.to_lowercase(),
                }
            }
            _ => self.reference.as_deref().unwrap_or("").trim().to_string(),
        };
        format!(
            "{}|{}|{}",
            self.source_type,
            self.repository
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_lowercase(),
            referent
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Actor {
    pub kind: String,
    pub id: String,
}

/// Who or what produced the entity.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Provenance {
    #[serde(alias = "createdBy")]
    pub created_by: Actor,
    #[serde(default, alias = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(
        default,
        alias = "lastModifiedBy",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_modified_by: Option<Actor>,
    #[serde(
        default,
        alias = "lastModifiedAt",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_modified_at: Option<String>,
    #[serde(
        default,
        alias = "agentSessionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_session_id: Option<String>,
}

/// Where a grounding was written in the markdown.
pub const GROUNDING_ORIGIN_FRONTMATTER: &str = "frontmatter";
pub const GROUNDING_ORIGIN_ANCHOR: &str = "anchor";

/// Baseline committed with one grounding reference in the markdown.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommittedGrounding {
    #[serde(rename = "ref")]
    pub reference: String,
    /// `frontmatter` (a metadata `grounds_to` entry) or `anchor` (an inline `kb-ground` anchor).
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// Legacy single-entity frontmatter shape (kept for API compatibility).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Frontmatter {
    pub id: Option<String>,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub entity_type: Option<String>,
    pub summary: Option<String>,
    pub status: Option<String>,
    pub revision: Option<i64>,
    #[serde(default)]
    pub relations: Vec<EntityRelation>,
    #[serde(default)]
    pub grounds_to: Vec<serde_json::Value>,
    #[serde(default)]
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiEntity {
    pub entity_key: String,
    pub id: String,
    pub file: String,
    pub entity_type: String,
    pub title: String,
    pub summary: Option<String>,
    pub body: String,
    /// Lifecycle state: in_flight | promoted | deprecated | archived.
    pub status: String,
    pub revision: i64,
    pub relations: Vec<EntityRelation>,
    pub grounds_to: Vec<String>,
    /// Baselines committed with the groundings in the markdown (`grounds_to` map entries and
    /// `#<body_hash>` anchor suffixes), one per reference in `grounds_to` order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_groundings: Vec<CommittedGrounding>,
    /// Topic references as written (ids, titles or aliases).
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<WikiSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Alternative names (topics resolve memberships through these).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Worst grounding health (fresh/unverified/ambiguous/changed/missing); None if ungrounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    /// 1-based line span of the entity in its file.
    #[serde(default)]
    pub start_line: usize,
    #[serde(default)]
    pub end_line: usize,
    /// Bound heading depth (0 for a file-level entity without a heading).
    #[serde(default)]
    pub heading_depth: usize,
    /// SHA-256 of the entity's own text (CRLF-normalized): the operation precondition.
    #[serde(default)]
    pub content_hash: String,
    /// `frontmatter`, `marker` or `implicit` (a file without entity frontmatter).
    #[serde(default)]
    pub metadata_kind: String,
}

impl WikiEntity {
    pub fn is_active(&self) -> bool {
        is_active_status(&self.status)
    }

    /// Committed baseline written for `reference`, if any.
    pub fn committed_for(&self, reference: &str) -> Option<&CommittedGrounding> {
        self.committed_groundings.iter().find(|c| c.reference == reference)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WikiDiagnostic {
    pub code: String,
    pub message: String,
    pub file: String,
    pub line: Option<usize>,
    /// error | warning | info
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// Field path inside the entity, e.g. `relations[2]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Precise span in `file` (byte offsets, 1-based line and UTF-16 column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<crate::wiki::positions::DiagnosticLocation>,
}

fn default_severity() -> String {
    "error".to_string()
}
