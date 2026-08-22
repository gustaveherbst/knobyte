//! Source positions for diagnostics, and the two range properties every Markdown edit keeps.
//!
//! Offsets are byte offsets into the decoded file text (the same offsets the scanner and the
//! operation planner use, so a diagnostic can be fed straight back into an edit). Lines are
//! 1-based; columns are 1-based and counted in UTF-16 code units, which is what editors and
//! the Language Server Protocol expect. Line terminators are `\n`, `\r\n` and a lone `\r`; a
//! leading byte-order mark is not part of column 1.
//!
//! The range checks mirror the write-scope guarantees: a set of entity ranges partitions a file
//! (no gap, no overlap, in order), and a planned mutation only changed the text inside the
//! ranges it declared.

use serde::{Deserialize, Serialize};

use crate::wiki::markdown::{line_starts, BOM};
use crate::wiki::models::WikiDiagnostic;
use crate::wiki::parser::EntityLocation;
use crate::wiki::yaml::top_level_keys;

/// Where a diagnostic points in its file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticLocation {
    /// Inclusive start (byte offset).
    pub start_offset: usize,
    /// Exclusive end (byte offset).
    pub end_offset: usize,
    /// 1-based line of `start_offset`.
    pub start_line: usize,
    /// 1-based column of `start_offset`, in UTF-16 code units.
    pub start_column: usize,
    /// 1-based line of `end_offset`.
    pub end_line: usize,
    /// 1-based column of `end_offset` (exclusive), in UTF-16 code units.
    pub end_column: usize,
}

/// A position map over one text: line starts computed once, positions on demand.
pub struct PositionMap<'a> {
    text: &'a str,
    starts: Vec<usize>,
    bom: usize,
}

impl<'a> PositionMap<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            starts: line_starts(text),
            bom: if text.starts_with(BOM) { BOM.len() } else { 0 },
        }
    }

    pub fn text(&self) -> &str {
        self.text
    }

    /// Number of lines (at least 1).
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Clamp `offset` into the text and back onto a character boundary.
    fn clamp(&self, offset: usize) -> usize {
        let mut o = offset.min(self.text.len());
        while o > 0 && !self.text.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    /// 1-based `(line, column)` of a byte offset; the column counts UTF-16 code units.
    pub fn line_column(&self, offset: usize) -> (usize, usize) {
        let offset = self.clamp(offset);
        let idx = match self.starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let mut line_start = self.starts[idx];
        if idx == 0 {
            line_start = line_start.max(self.bom.min(offset));
        }
        let column = self.text[line_start..offset].encode_utf16().count() + 1;
        (idx + 1, column)
    }

    /// Byte range `[start, end)` of line `line` (1-based) without its terminator.
    pub fn line_span(&self, line: usize) -> Option<(usize, usize)> {
        let idx = line.checked_sub(1)?;
        let start = *self.starts.get(idx)?;
        let next = self.starts.get(idx + 1).copied().unwrap_or(self.text.len());
        let content_end = start + self.text[start..next].trim_end_matches(['\n', '\r']).len();
        let start = if idx == 0 {
            start.max(self.bom).min(content_end)
        } else {
            start
        };
        Some((start, content_end))
    }

    /// Location of the byte range `[start, end)`.
    pub fn location(&self, start: usize, end: usize) -> DiagnosticLocation {
        let start = self.clamp(start);
        let end = self.clamp(end.max(start));
        let (start_line, start_column) = self.line_column(start);
        let (end_line, end_column) = self.line_column(end);
        DiagnosticLocation {
            start_offset: start,
            end_offset: end,
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    /// Location of a line's visible content (leading whitespace excluded).
    pub fn line_location(&self, line: usize) -> Option<DiagnosticLocation> {
        let (s, e) = self.line_span(line)?;
        let content = &self.text[s..e];
        let lead = content.len() - content.trim_start().len();
        Some(self.location(s + lead, e.max(s + lead)))
    }
}

/// The key a structural path names: `relations[2].target` -> `relations`, `status` -> `status`.
fn path_key(path: &str) -> &str {
    let end = path.find(['[', '.']).unwrap_or(path.len());
    &path[..end]
}

/// The first list index in a structural path: `relations[2]` -> 2.
fn path_index(path: &str) -> Option<usize> {
    let open = path.find('[')?;
    let close = path[open..].find(']')? + open;
    path[open + 1..close].parse().ok()
}

/// Span of a metadata key (or one item of its list) inside an entity's YAML region.
fn key_span(text: &str, loc: &EntityLocation, path: &str) -> Option<(usize, usize)> {
    if loc.yaml_end <= loc.yaml_start || loc.yaml_end > text.len() {
        return None;
    }
    let key = path_key(path);
    let range = top_level_keys(text, loc.yaml_start, loc.yaml_end)
        .into_iter()
        .find(|k| k.key == key)?;
    if let Some(i) = path_index(path) {
        // Block list items: `- ...` lines (any indentation) under the key.
        let region = &text[range.key_start..range.value_end];
        let mut seen = 0usize;
        let mut offset = range.key_start;
        for line in region.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if offset != range.key_start && (trimmed.starts_with("- ") || trimmed.trim_end() == "-")
            {
                if seen == i {
                    let lead = line.len() - trimmed.len();
                    let content = trimmed.trim_end_matches(['\n', '\r']);
                    return Some((offset + lead, offset + lead + content.len()));
                }
                seen += 1;
            }
            offset += line.len();
        }
    }
    Some((range.key_start, range.value_end))
}

/// Give a diagnostic a precise location in `text`: the metadata key its `path` names (inside
/// `entity`'s metadata), else its recorded line, else the entity's heading. A diagnostic that
/// already has a location is left alone. The 1-based `line` is filled from the location.
pub fn locate_diagnostic(
    d: &mut WikiDiagnostic,
    map: &PositionMap,
    entity: Option<&EntityLocation>,
) {
    if d.location.is_some() {
        return;
    }
    let text = map.text();
    let span = entity
        .zip(d.path.as_deref())
        .and_then(|(loc, path)| key_span(text, loc, path));
    let location = match span {
        Some((s, e)) => Some(map.location(s, e)),
        None => match d.line {
            Some(l) if l >= 1 && l <= map.line_count() => map.line_location(l),
            _ => entity.and_then(|loc| {
                if loc.heading_end > loc.heading_start {
                    let heading = &text[loc.heading_start..loc.heading_end];
                    let content = heading.trim_end_matches(['\n', '\r']);
                    Some(map.location(loc.heading_start, loc.heading_start + content.len()))
                } else if loc.metadata_end > loc.metadata_start {
                    Some(map.location(loc.metadata_start, loc.metadata_end))
                } else {
                    None
                }
            }),
        },
    };
    if let Some(l) = location {
        d.line = Some(l.start_line);
        d.location = Some(l);
    }
}

// ---------------------------------------------------------------------------
// Range properties
// ---------------------------------------------------------------------------

/// A labelled byte range `[start, end)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledRange {
    pub start: usize,
    pub end: usize,
    pub label: String,
}

impl LabeledRange {
    pub fn new(start: usize, end: usize, label: impl Into<String>) -> Self {
        Self {
            start,
            end,
            label: label.into(),
        }
    }

    fn describe(&self) -> String {
        format!("{} [{}, {})", self.label, self.start, self.end)
    }
}

fn excerpt(text: &str, start: usize) -> String {
    let mut end = (start + 32).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let slice = &text[start.min(end)..end];
    format!("{:?}{}", slice, if end < text.len() { "…" } else { "" })
}

/// Property 1: `ranges` partition `source` — in position order, no overlap, no unaccounted gap.
pub fn check_range_partition(source: &str, ranges: &[LabeledRange]) -> Result<(), String> {
    for r in ranges {
        if r.end < r.start {
            return Err(format!("{} is inverted: end precedes start", r.describe()));
        }
        if r.end > source.len() {
            return Err(format!(
                "{} runs past the end of the input (length {})",
                r.describe(),
                source.len()
            ));
        }
    }
    if ranges.is_empty() {
        return if source.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "No ranges supplied, but the input is {} bytes long",
                source.len()
            ))
        };
    }
    for w in ranges.windows(2) {
        if w[1].start < w[0].start {
            return Err(format!(
                "{} is supplied after {} but starts earlier; ranges must be in position order",
                w[1].describe(),
                w[0].describe()
            ));
        }
    }
    let mut cursor = 0;
    for r in ranges {
        if r.start < cursor {
            return Err(format!(
                "{} overlaps the previous range, which ended at {}",
                r.describe(),
                cursor
            ));
        }
        if r.start > cursor {
            return Err(format!(
                "Unaccounted text at [{}, {}) before {}: {}",
                cursor,
                r.start,
                r.describe(),
                excerpt(source, cursor)
            ));
        }
        cursor = r.end;
    }
    if cursor != source.len() {
        return Err(format!(
            "Unaccounted text at [{}, {}) after the last range: {}",
            cursor,
            source.len(),
            excerpt(source, cursor)
        ));
    }
    Ok(())
}

/// Property 2: `produced` differs from `original` only inside the `declared` ranges of the
/// original: every byte outside them survives, in order.
pub fn check_only_ranges_changed(
    original: &str,
    produced: &str,
    declared: &[LabeledRange],
) -> Result<(), String> {
    let mut sorted = declared.to_vec();
    sorted.sort_by_key(|r| (r.start, r.end));
    for r in &sorted {
        if r.end < r.start || r.end > original.len() {
            return Err(format!(
                "{} lies outside the original text (length {})",
                r.describe(),
                original.len()
            ));
        }
    }
    for w in sorted.windows(2) {
        if w[1].start < w[0].end {
            return Err(format!(
                "Declared ranges overlap: {} and {}",
                w[0].describe(),
                w[1].describe()
            ));
        }
    }
    // The complement segments must appear in `produced`, in order, with the head anchored at
    // the start and the tail anchored at the end.
    let mut segments: Vec<&str> = Vec::new();
    let mut cursor = 0;
    for r in &sorted {
        segments.push(&original[cursor..r.start]);
        cursor = r.end;
    }
    segments.push(&original[cursor..]);
    if sorted.is_empty() {
        return if original == produced {
            Ok(())
        } else {
            Err("No ranges were declared, so the text had to be unchanged".to_string())
        };
    }
    let head = segments[0];
    if !produced.starts_with(head) {
        return Err(format!("Text before {} changed", sorted[0].describe()));
    }
    let tail = segments[segments.len() - 1];
    if !produced.ends_with(tail) || produced.len() < head.len() + tail.len() {
        return Err(format!(
            "Text after {} changed",
            sorted[sorted.len() - 1].describe()
        ));
    }
    let mut pos = head.len();
    let limit = produced.len() - tail.len();
    for (i, seg) in segments[1..segments.len() - 1].iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        match produced[pos..limit].find(seg) {
            Some(found) => pos += found + seg.len(),
            None => {
                return Err(format!(
                    "Text between {} and {} changed",
                    sorted[i].describe(),
                    sorted[i + 1].describe()
                ))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_count_utf16_and_skip_the_bom() {
        let text = "\u{feff}ab\r\nc\u{1F600}d\re";
        let map = PositionMap::new(text);
        assert_eq!(map.line_column(3), (1, 1));
        assert_eq!(map.line_column(4), (1, 2));
        let d = text.find('d').unwrap();
        assert_eq!(map.line_column(d), (2, 4));
        let e = text.find('e').unwrap();
        assert_eq!(map.line_column(e), (3, 1));
        let loc = map.line_location(2).unwrap();
        assert_eq!(
            (loc.start_line, loc.start_column, loc.end_column),
            (2, 1, 5)
        );
    }

    #[test]
    fn partition_and_scoped_mutation() {
        let src = "aaabbbccc";
        let parts = vec![
            LabeledRange::new(0, 3, "a"),
            LabeledRange::new(3, 6, "b"),
            LabeledRange::new(6, 9, "c"),
        ];
        assert!(check_range_partition(src, &parts).is_ok());
        assert!(check_range_partition(src, &parts[..2])
            .unwrap_err()
            .contains("Unaccounted"));
        let swapped = vec![parts[1].clone(), parts[0].clone()];
        assert!(check_range_partition(src, &swapped)
            .unwrap_err()
            .contains("position order"));
        let declared = vec![LabeledRange::new(3, 6, "b")];
        assert!(check_only_ranges_changed(src, "aaaXYZXYZccc", &declared).is_ok());
        assert!(check_only_ranges_changed(src, "aaXbbbccc", &declared).is_err());
        assert!(check_only_ranges_changed(src, "aaabbbcc", &declared).is_err());
    }
}
