//! Scoped edits of top-level YAML keys inside a frontmatter block or an entity marker.
//!
//! Only the targeted key's lines are rewritten; comments, key order, quoting and every other
//! key in the block are preserved byte for byte.

use crate::wiki::markdown::{split_lines, Edit};

#[derive(Debug, Clone, PartialEq)]
pub struct KeyRange {
    pub key: String,
    pub key_start: usize,
    /// End of the value's last content line (no terminator).
    pub value_end: usize,
    /// End of the value including its line terminator.
    pub node_end: usize,
}

fn top_level_key_name(line: &str) -> Option<String> {
    let first = *line.as_bytes().first()?;
    if matches!(
        first,
        b' ' | b'\t' | b'#' | b'-' | b']' | b'}' | b'\'' | b'"'
    ) {
        // Quoted keys are unusual in frontmatter; treat them as continuation for safety.
        if first == b'"' || first == b'\'' {
            let q = first as char;
            let rest = &line[1..];
            let close = rest.find(q)?;
            let after = &rest[close + 1..];
            if after.starts_with(':') && (after.len() == 1 || after[1..].starts_with([' ', '\t'])) {
                return Some(rest[..close].to_string());
            }
        }
        return None;
    }
    let colon = line.find(':')?;
    let key = &line[..colon];
    let after = &line[colon + 1..];
    if !(after.is_empty() || after.starts_with([' ', '\t'])) {
        return None;
    }
    if key.contains(['#', '{', '[', ',']) || key.trim() != key {
        return None;
    }
    Some(key.to_string())
}

/// Top-level keys of the YAML text in `[start, end)` of `text`, in order.
pub fn top_level_keys(text: &str, start: usize, end: usize) -> Vec<KeyRange> {
    let region = &text[start..end];
    let lines = split_lines(region);
    let mut out: Vec<KeyRange> = Vec::new();
    let mut current: Option<KeyRange> = None;
    for l in &lines {
        let line = &region[l.start..l.content_end];
        let blank = line.trim().is_empty();
        let col0 = !blank && !line.starts_with([' ', '\t']);
        if col0 {
            if let Some(name) = top_level_key_name(line) {
                if let Some(c) = current.take() {
                    out.push(c);
                }
                current = Some(KeyRange {
                    key: name,
                    key_start: start + l.start,
                    value_end: start + l.content_end,
                    node_end: start + l.end,
                });
                continue;
            }
            let first = line.as_bytes()[0];
            if first == b'#' {
                // A column-0 comment closes the current node (it belongs to what follows).
                if let Some(c) = current.take() {
                    out.push(c);
                }
                continue;
            }
        }
        if let Some(c) = current.as_mut() {
            if !blank && !line.trim_start().starts_with('#') {
                c.value_end = start + l.content_end;
                c.node_end = start + l.end;
            }
        }
    }
    if let Some(c) = current.take() {
        out.push(c);
    }
    out
}

pub fn find_key(text: &str, start: usize, end: usize, key: &str) -> Option<KeyRange> {
    top_level_keys(text, start, end)
        .into_iter()
        .find(|k| k.key == key)
}

/// Render `key: value` as block YAML using `eol`, without a trailing terminator.
pub fn render_key_value(key: &str, value: &serde_yaml::Value, eol: &str) -> String {
    let mut map = serde_yaml::Mapping::new();
    map.insert(serde_yaml::Value::String(key.to_string()), value.clone());
    let rendered = serde_yaml::to_string(&map).unwrap_or_else(|_| format!("{}: null\n", key));
    let rendered = rendered.trim_end_matches('\n');
    if eol == "\n" {
        rendered.to_string()
    } else {
        rendered.replace('\n', eol)
    }
}

/// Leading whitespace of the first block-sequence item (`- ...`) in `[start, end)`, when the
/// item is indented under its key (`key:\n  - a`); `None` for the flush style or no list.
fn sequence_indent(text: &str, start: usize, end: usize) -> Option<String> {
    let region = &text[start..end];
    for l in split_lines(region) {
        let line = &region[l.start..l.content_end];
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed == "-" || trimmed.starts_with("- ") {
            let indent = &line[..line.len() - trimmed.len()];
            return (!indent.is_empty()).then(|| indent.to_string());
        }
    }
    None
}

/// Indent every continuation line of a rendered block value (a `key:` followed by lines),
/// so a rewritten list keeps the file's `key:\n  - item` style.
fn indent_block(rendered: &str, indent: &str, eol: &str) -> String {
    let mut lines = rendered.split(eol);
    let mut out = lines.next().unwrap_or("").to_string();
    for l in lines {
        out.push_str(eol);
        if !l.is_empty() {
            out.push_str(indent);
        }
        out.push_str(l);
    }
    out
}

/// Edit that sets `key` to `value` within the YAML region `[start, end)`. A list keeps the
/// indentation style the key (or, for a new key, the block) already uses.
pub fn set_key_edit(
    text: &str,
    start: usize,
    end: usize,
    key: &str,
    value: &serde_yaml::Value,
    eol: &str,
) -> Edit {
    let mut rendered = render_key_value(key, value, eol);
    let keys = top_level_keys(text, start, end);
    let existing = keys.iter().find(|k| k.key == key);
    if matches!(value, serde_yaml::Value::Sequence(s) if !s.is_empty()) {
        let indent = match existing {
            Some(k) => sequence_indent(text, k.key_start, k.node_end)
                .or_else(|| sequence_indent(text, start, end)),
            None => sequence_indent(text, start, end),
        };
        if let Some(indent) = indent {
            rendered = indent_block(&rendered, &indent, eol);
        }
    }
    if let Some(k) = existing {
        return Edit {
            start: k.key_start,
            end: k.value_end,
            text: rendered,
            label: format!("metadata key {}", key),
        };
    }
    match keys.last() {
        Some(last) => Edit {
            start: last.value_end,
            end: last.value_end,
            text: format!("{}{}", eol, rendered),
            label: format!("insert metadata key {}", key),
        },
        None => {
            // Empty region: insert at its start, terminated.
            Edit {
                start,
                end: start,
                text: format!("{}{}", rendered, eol),
                label: format!("insert metadata key {}", key),
            }
        }
    }
}

/// Edit that removes `key` (and its value lines) from the region; None when absent.
pub fn remove_key_edit(text: &str, start: usize, end: usize, key: &str) -> Option<Edit> {
    let k = find_key(text, start, end, key)?;
    Some(Edit {
        start: k.key_start,
        end: k.node_end,
        text: String::new(),
        label: format!("remove metadata key {}", key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::markdown::apply_edits;

    #[test]
    fn splices_one_key_and_keeps_the_rest() {
        let text = "id: kb_a\n# keep me\ntitle: \"A\"\nrelations:\n- type: depends_on\n  target_id: kb_b\nrevision: 1\n";
        let keys = top_level_keys(text, 0, text.len());
        let names: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(names, vec!["id", "title", "relations", "revision"]);
        let e = set_key_edit(
            text,
            0,
            text.len(),
            "revision",
            &serde_yaml::Value::from(2),
            "\n",
        );
        let out = apply_edits(text, &[e]).unwrap();
        assert!(out.ends_with("revision: 2\n"));
        assert!(out.contains("# keep me\ntitle: \"A\""));
        let e = set_key_edit(
            text,
            0,
            text.len(),
            "status",
            &serde_yaml::Value::from("promoted"),
            "\n",
        );
        let out = apply_edits(text, &[e]).unwrap();
        assert!(out.ends_with("revision: 1\nstatus: promoted\n"), "{}", out);
        // A list keeps its indented style when rewritten.
        let indented = "id: kb_a\ngrounds_to:\n  - node_id: a\n  - node_id: b\nrevision: 1\n";
        let list = serde_yaml::Value::Sequence(vec!["x".into(), "y".into()]);
        let e = set_key_edit(indented, 0, indented.len(), "grounds_to", &list, "\n");
        let out = apply_edits(indented, &[e]).unwrap();
        assert_eq!(out, "id: kb_a\ngrounds_to:\n  - x\n  - y\nrevision: 1\n");
        let e = set_key_edit(text, 0, text.len(), "aliases", &list, "\n");
        let out = apply_edits(text, &[e]).unwrap();
        assert!(out.ends_with("revision: 1\naliases:\n- x\n- y\n"), "{}", out);
        let e = remove_key_edit(text, 0, text.len(), "relations").unwrap();
        let out = apply_edits(text, &[e]).unwrap();
        assert_eq!(out, "id: kb_a\n# keep me\ntitle: \"A\"\nrevision: 1\n");
    }
}
