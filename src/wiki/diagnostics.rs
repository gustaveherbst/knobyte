//! Diagnostic code registry: every wiki check reports through a stable code with a default
//! severity and a remediation hint. Validators never abort on the first problem.

use crate::wiki::models::WikiDiagnostic;

/// `(code, default severity, remediation)`.
pub const DIAGNOSTIC_CODES: &[(&str, &str, &str)] = &[
    ("INVALID_ENTITY_ID", "error", "Entity ids are a single token of letters, digits, '_', '-', '.' or ':' (Knobyte mints `kb_<slug>`)."),
    ("DUPLICATE_ENTITY_ID", "error", "Two entities claim one id. Keep the original and give the copy a new id."),
    ("INVALID_ENTITY_TYPE", "error", "Use a registered entity type, or register the type under `wiki.entityTypes` in config.json."),
    ("INVALID_LIFECYCLE_STATE", "error", "Lifecycle is one of in_flight, promoted, deprecated, archived. Grounding health is a separate field."),
    ("LEGACY_LIFECYCLE_STATE", "info", "This status was mapped onto a lifecycle state. Write the lifecycle value (in_flight, promoted, deprecated, archived) to make it explicit."),
    ("INVALID_REVISION", "error", "Revision is an integer starting at 1 that only ever increases."),
    ("MISSING_ENTITY_TITLE", "error", "Give the entity a heading, or set an explicit title."),
    ("MISSING_REQUIRED_FIELD", "warning", "Add the missing field."),
    ("INVALID_FIELD_TYPE", "error", "Correct the field's type."),
    ("REVISION_DIVERGED", "info", "The entity was edited by hand since its revision was recorded. Bump the revision (or apply an operation) to adopt the current text."),
    ("INVALID_RELATION_TYPE", "error", "Use one of the registered relation types."),
    ("INVALID_RELATION_TARGET", "error", "Point the relation at an entity id that exists in the wiki."),
    ("DUPLICATE_RELATION", "error", "Remove the repeated (source, type, target) triple; one relation carries the meaning."),
    ("SELF_RELATION", "error", "An entity cannot relate to itself. Point the relation at the other entity."),
    ("SUPERSESSION_CYCLE", "error", "Break the supersedes cycle; supersession must form a chain, not a loop."),
    ("CONTRADICTORY_ACTIVE_DECISIONS", "error", "Deprecate the superseded decision, or waive the contradiction explicitly with `waived: true` on the relation."),
    ("INACTIVE_RELATION_TARGET", "warning", "This active entity depends on a deprecated or archived one. Retarget it, or retire the source too."),
    ("ORPHANED_ENTITY", "info", "A promoted entity with no relations is hard to find. Relate it to a topic or a neighbouring entity."),
    ("UNKNOWN_TOPIC", "error", "Create the topic entity, or point the membership at an existing topic id, title or alias."),
    ("AMBIGUOUS_TOPIC_REFERENCE", "error", "Several topics share this name or alias. Use the topic's id instead."),
    ("INVALID_TOPIC_MEMBER", "error", "Topic membership must reference an entity of type `topic`."),
    ("TOPIC_CYCLE", "error", "Break the parent-topic cycle; the topic hierarchy must be acyclic."),
    ("MALFORMED_SOURCE", "error", "Fill in the fields this source kind requires."),
    ("INVALID_COMMIT_FORMAT", "error", "A commit reference is a hexadecimal SHA of 7-40 characters."),
    ("DUPLICATE_SOURCE", "warning", "Remove the repeated evidence entry."),
    ("UNRESOLVED_EXTERNAL_SOURCE", "info", "This evidence points outside the repository and has not been resolved. That is legal; it is recorded so it is not mistaken for verified."),
    ("MALFORMED_GROUNDING", "error", "A grounding is a readable reference `kind:path:qualified_name` (or a graph node id)."),
    ("GROUNDING_UNVERIFIED", "error", "Groundings written by operations must resolve in the live code graph. Build the graph (`knobyte graph rebuild`) and use a reference it can resolve."),
    ("GROUNDING_MIXED_SHAPE", "info", "Groundings are kept in more than one shape; prefer readable references in `grounds_to`."),
    ("INVALID_OPERATION_ENVELOPE", "error", "Correct the operation envelope; it carries an opId, type, actor and timestamp."),
    ("UNKNOWN_OPERATION_TYPE", "error", "Use one of the eleven registered operation types."),
    ("INVALID_OPERATION_PAYLOAD", "error", "Correct the payload for this operation type."),
    ("REVISION_CONFLICT", "error", "The entity moved on since this operation was planned. Re-read it and retry."),
    ("CONTENT_HASH_CONFLICT", "error", "The entity's text changed since this operation was planned. Re-read it and retry."),
    ("WIKI_INDEX_MISSING", "error", "Run `knobyte wiki rebuild-index`."),
    ("WIKI_INDEX_REBUILD_REQUIRED", "error", "The index was built by a different schema version. Run `knobyte wiki rebuild-index`."),
    ("WIKI_CORPUS_LIMIT_EXCEEDED", "error", "The wiki corpus exceeded a safety bound. Exclude generated or vendored Markdown with `wiki.exclude`."),
    ("WIKI_PARSE_ERROR", "error", "Fix the malformed Markdown or entity metadata block. Prose is never deleted to resolve this."),
    ("FRONTMATTER_PARSE_ERROR", "error", "Fix the YAML frontmatter; until then its fields (id, relations, grounds_to, ...) are ignored."),
    ("FRONTMATTER_UNTERMINATED", "error", "Close the frontmatter with a `---` line."),
    ("UNBOUND_ENTITY_METADATA", "error", "An entity marker must be followed by a heading, with only blank lines between."),
    ("DUPLICATE_ENTITY_METADATA", "error", "Two entity markers bind to one heading. Remove one, or give the second its own heading."),
    ("ENTITY_RANGE_OVERLAP", "error", "Two entities claim overlapping regions of one file. Re-check the heading depths."),
    ("MERGE_CONFLICT_MARKERS", "warning", "Resolve the merge conflict; headings and entity markers inside it are ignored."),
    ("PATH_OUTSIDE_SCAFFOLD", "warning", "Point the path inside the scaffold. Files outside the scaffold root are never indexed or written."),
    ("ENTITY_NOT_FOUND", "error", "No entity has that id. Check the id, or rebuild the index."),
    ("GROUNDING_UNRESOLVED", "warning", "The code graph could not resolve this reference. Fix the reference, or run `knobyte sync` if the symbol moved."),
    ("GROUNDING_STALE", "warning", "The grounded code changed since its baseline. Review the entity, then run `knobyte graph ground` to accept the current code."),
    ("GROUNDING_MISSING", "warning", "The grounded declaration no longer exists. Re-ground the entity (`knobyte sync`) or retire it."),
    ("AMBIGUOUS_GROUNDING", "warning", "The reference matches several symbols; qualify it."),
    ("GROUNDINGS_UNCHECKED", "warning", "Groundings were not verified because the code graph is not built. Run `knobyte graph rebuild`."),
    ("SOURCE_FILE_MISSING", "warning", "The file this evidence names is gone. Update or remove the source."),
    ("UNBOUND_ANCHOR", "warning", "A `kb-ground` anchor sits outside every entity (before the first heading of a file without frontmatter). Move it into the section it grounds."),
    ("ANCHOR_GROUNDING_MISMATCH", "warning", "An inline `kb-ground` anchor disagrees with the entity's declared grounding. Reconcile the two."),
    ("AMBIGUOUS_MIGRATION", "warning", "The target moved since this was planned. Re-plan rather than annotating the wrong section."),
    ("WRITE_SCOPE_VIOLATION", "error", "The write touched text outside its declared range, or targeted a read-only path. Nothing was written."),
    ("INDEX_REFRESH_REQUIRED", "warning", "The Markdown write succeeded but the index did not refresh. Run `knobyte wiki rebuild-index`."),
    ("MALFORMED_OPERATION_LOG", "warning", "A line in events/operations.jsonl is not valid JSON. The Markdown is unaffected."),
    ("GENERATED_VIEW_DRIFT", "info", "A generated section no longer matches the wiki. Run `knobyte wiki regenerate-views`."),
    ("WIKI_INDEX_CORRUPT", "error", "The wiki index failed its integrity check. Run `knobyte wiki rebuild-index`; the Markdown is unaffected."),
    ("WIKI_INDEX_BUSY", "error", "Another process holds the wiki index maintenance lease. Retry when it finishes."),
    ("OPERATION_INTERRUPTED", "error", "Index maintenance was interrupted (aborted, or the Markdown changed while it ran). Nothing was published; run it again."),
    ("WIKI_MIGRATION_REQUIRED", "warning", "The wiki holds older Knobyte formats. Run `knobyte wiki migrate --apply`."),
    ("MIGRATION_ABSTAINED", "warning", "Migration left this entity for review; fix it by hand or re-run after the cause is resolved."),
    ("INVALID_REQUEST", "error", "Correct the request (argument shape, bounds or cursor)."),
    ("PLAN_HANDLE_INVALID", "error", "The plan handle is unknown, expired or already used. Plan the operation again."),
    ("INVALID_AGENT_RESPONSE", "error", "The response must be JSON carrying the array this stage expects: `units`, `actions` or `judgments`."),
];

pub fn definition(code: &str) -> Option<(&'static str, &'static str)> {
    DIAGNOSTIC_CODES
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, s, r)| (*s, *r))
}

pub fn is_known_code(code: &str) -> bool {
    definition(code).is_some()
}

/// Build a diagnostic with the registry's default severity and remediation.
pub fn diag(code: &str, message: impl Into<String>, file: impl Into<String>) -> WikiDiagnostic {
    let (severity, remediation) = definition(code).unwrap_or(("error", ""));
    WikiDiagnostic {
        code: code.to_string(),
        message: message.into(),
        file: file.into(),
        line: None,
        severity: severity.to_string(),
        entity_id: None,
        path: None,
        remediation: if remediation.is_empty() {
            None
        } else {
            Some(remediation.to_string())
        },
        location: None,
    }
}

/// Builder helpers.
pub trait DiagExt {
    fn at_line(self, line: Option<usize>) -> Self;
    fn for_entity(self, id: &str) -> Self;
    fn at_path(self, path: impl Into<String>) -> Self;
    fn severity(self, severity: &str) -> Self;
}

impl DiagExt for WikiDiagnostic {
    fn at_line(mut self, line: Option<usize>) -> Self {
        self.line = line;
        self
    }
    fn for_entity(mut self, id: &str) -> Self {
        self.entity_id = Some(id.to_string());
        self
    }
    fn at_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
    fn severity(mut self, severity: &str) -> Self {
        self.severity = severity.to_string();
        self
    }
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    }
}

/// Deterministic order: severity, file, line, code, entity, path, message.
pub fn sort_diagnostics(diags: &mut [WikiDiagnostic]) {
    diags.sort_by(|a, b| {
        severity_rank(&a.severity)
            .cmp(&severity_rank(&b.severity))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.unwrap_or(0).cmp(&b.line.unwrap_or(0)))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.entity_id.cmp(&b.entity_id))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.message.cmp(&b.message))
    });
}

/// Remove exact duplicates, keeping the first occurrence.
pub fn dedupe_diagnostics(diags: Vec<WikiDiagnostic>) -> Vec<WikiDiagnostic> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for d in diags {
        let key = format!(
            "{}|{}|{}|{}|{:?}|{:?}|{:?}",
            d.code, d.severity, d.message, d.file, d.entity_id, d.path, d.line
        );
        if seen.insert(key) {
            out.push(d);
        }
    }
    out
}

pub fn has_errors(diags: &[WikiDiagnostic]) -> bool {
    diags.iter().any(|d| d.severity == "error")
}
