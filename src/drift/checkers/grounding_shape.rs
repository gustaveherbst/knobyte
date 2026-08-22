//! GROUNDING_MIXED_SHAPE: a file whose `grounds_to` keeps groundings in more than one shape.
//!
//! Knobyte accepts plain references (`kind:path:qualified_name`), committed-baseline maps
//! (`{ref, body_hash, fingerprint}`, the same shape as a plain reference once baselined),
//! `node_id:` mappings and legacy hashed graph ids. All are read, so mixing them is not a missed grounding (info).
//! A `node_id:` mapping whose own `file_path` / `symbol` fields name a different location than
//! its reference is a real disagreement (warning): the reference is what gets checked.
//! Needs no graph.

use crate::drift::types::{codes, DriftIssue, SEVERITY_INFO, SEVERITY_WARNING};
use crate::graph::grounding::{parse_grounding_ref, ParsedRef};

/// [`check_grounding_shape`] over a whole markdown document (for callers such as
/// `knobyte wiki validate` that hold the text rather than parsed frontmatter).
pub fn check_grounding_shape_in(content: &str, source: &str) -> Vec<DriftIssue> {
    let fm = crate::drift::markdown::parse_frontmatter(content);
    check_grounding_shape(fm.as_ref(), source)
}

pub fn check_grounding_shape(
    frontmatter: Option<&serde_json::Value>,
    source: &str,
) -> Vec<DriftIssue> {
    let Some(entries) = frontmatter
        .and_then(|fm| fm.get("grounds_to"))
        .and_then(|g| g.as_array())
    else {
        return Vec::new();
    };

    let mut plain = 0;
    let mut mapped = 0;
    let mut legacy = 0;
    let mut readable = 0;
    let mut conflicts: Vec<String> = Vec::new();

    for entry in entries {
        let reference = if let Some(s) = entry.as_str() {
            plain += 1;
            s.trim().to_string()
        } else if let Some(r) = entry.get("ref").and_then(|v| v.as_str()) {
            // A plain reference with its committed baseline.
            plain += 1;
            r.trim().to_string()
        } else if let Some(id) = entry.get("node_id").and_then(|v| v.as_str()) {
            mapped += 1;
            let id = id.trim().to_string();
            if let ParsedRef::Readable(r) = parse_grounding_ref(&id) {
                let file_conflict = entry
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|f| f.trim().trim_start_matches("./") != r.file_path)
                    .unwrap_or(false);
                let symbol_conflict = entry
                    .get("symbol")
                    .and_then(|v| v.as_str())
                    .map(|s| {
                        let s = s.trim();
                        s != r.qualified_name && s != r.symbol_name()
                    })
                    .unwrap_or(false);
                if file_conflict || symbol_conflict {
                    conflicts.push(id.clone());
                }
            }
            id
        } else {
            continue;
        };
        match parse_grounding_ref(&reference) {
            ParsedRef::LegacyId(_) => legacy += 1,
            ParsedRef::Readable(_) => readable += 1,
            ParsedRef::Other(_) => {}
        }
    }

    if !conflicts.is_empty() {
        return vec![DriftIssue::new(
            codes::GROUNDING_MIXED_SHAPE,
            SEVERITY_WARNING,
            source,
            None,
            format!(
                "`grounds_to` mappings name a different location than their reference: {}. The reference is checked; keep the right one and fix the other fields.",
                conflicts.join(", ")
            ),
        )];
    }

    let mut parts = Vec::new();
    if plain > 0 && mapped > 0 {
        parts.push("plain references and `node_id:` mappings");
    }
    if legacy > 0 && readable > 0 {
        parts.push("legacy hashed ids and readable `kind:path:qualified_name` references");
    }
    if parts.is_empty() {
        return Vec::new();
    }
    vec![DriftIssue::new(
        codes::GROUNDING_MIXED_SHAPE,
        SEVERITY_INFO,
        source,
        None,
        format!(
            "Groundings are split between {}; all are checked. `knobyte sync` rewrites relocated legacy ids to readable references.",
            parts.join(" and between ")
        ),
    )]
}
