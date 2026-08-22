//! Structural scan of a wiki Markdown file: frontmatter, headings (ATX and setext), entity
//! markers, grounding anchors, fenced code blocks and merge-conflict regions.
//!
//! All offsets are byte offsets into the file text. The scanner is BOM-, CRLF- and
//! code-fence-safe: nothing inside a fenced code block, an HTML comment or a merge-conflict
//! region is mistaken for a heading or an entity marker.

/// One physical line: `[start, content_end)` is the text, `[content_end, end)` the terminator.
#[derive(Debug, Clone, Copy)]
pub struct Line {
    pub start: usize,
    pub content_end: usize,
    pub end: usize,
}

pub fn split_lines(text: &str) -> Vec<Line> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let content_end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            lines.push(Line {
                start,
                content_end,
                end: i + 1,
            });
            start = i + 1;
        } else if bytes[i] == b'\r' && bytes.get(i + 1) != Some(&b'\n') {
            lines.push(Line {
                start,
                content_end: i,
                end: i + 1,
            });
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        lines.push(Line {
            start,
            content_end: bytes.len(),
            end: bytes.len(),
        });
    }
    lines
}

/// Byte offsets where each line starts (for 1-based line lookups).
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for l in split_lines(text) {
        if l.end < text.len() {
            starts.push(l.end);
        }
    }
    starts
}

/// 1-based line number containing `offset`.
pub fn line_at(starts: &[usize], offset: usize) -> usize {
    match starts.binary_search(&offset) {
        Ok(i) => i + 1,
        Err(i) => i.max(1),
    }
}

/// `\r\n` when the text uses CRLF anywhere, else `\n`.
pub fn dominant_eol(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

pub const BOM: &str = "\u{feff}";

#[derive(Debug, Clone)]
pub struct FrontmatterBlock {
    /// Start of the opening `---` line.
    pub start: usize,
    /// End of the closing line, including its terminator.
    pub end: usize,
    /// YAML text region.
    pub inner_start: usize,
    pub inner_end: usize,
}

#[derive(Debug, Clone)]
pub struct Heading {
    pub depth: usize,
    pub start: usize,
    /// End including the line terminator.
    pub end: usize,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct EntityMarker {
    pub start: usize,
    /// End of the marker (after `-->`, plus the rest of that line when blank).
    pub end: usize,
    /// Inline `key=value` attributes written on the marker's first line.
    pub attrs: String,
    /// YAML region (lines between the first line and `-->`).
    pub yaml_start: usize,
    pub yaml_end: usize,
}

#[derive(Debug, Clone)]
pub struct Anchor {
    pub start: usize,
    pub end: usize,
    pub reference: String,
    /// Committed baseline body hash (`#<body_hash>` suffix), if any.
    pub body_hash: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanProblem {
    pub code: &'static str,
    pub message: String,
    pub offset: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ScannedDoc {
    /// Length of a leading byte-order mark (0 or 3).
    pub bom: usize,
    pub frontmatter: Option<FrontmatterBlock>,
    pub headings: Vec<Heading>,
    pub markers: Vec<EntityMarker>,
    pub anchors: Vec<Anchor>,
    pub conflicts: Vec<(usize, usize)>,
    pub problems: Vec<ScanProblem>,
}

const MARKER_KEYWORD: &str = "kb:entity";
const ANCHOR_PREFIXES: [&str; 3] = ["<!-- kb-ground:", "<!-- grounds:", "<!-- kb-anchor:"];

fn leading_spaces(s: &str) -> usize {
    s.bytes().take_while(|b| *b == b' ').count()
}

pub(crate) fn fence_open(line: &str) -> Option<(u8, usize)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = *rest.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let count = rest.bytes().take_while(|b| *b == ch).count();
    if count < 3 {
        return None;
    }
    if ch == b'`' && rest[count..].contains('`') {
        return None;
    }
    Some((ch, count))
}

pub(crate) fn fence_closes(line: &str, ch: u8, count: usize) -> bool {
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let n = rest.bytes().take_while(|b| *b == ch).count();
    n >= count && rest[n..].trim().is_empty()
}

fn atx_heading(line: &str) -> Option<(usize, String)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let depth = rest.bytes().take_while(|b| *b == b'#').count();
    if depth == 0 || depth > 6 {
        return None;
    }
    let after = &rest[depth..];
    if !after.is_empty() && !after.starts_with(' ') && !after.starts_with('\t') {
        return None;
    }
    let mut title = after.trim().to_string();
    // Strip an optional closing sequence of '#'.
    let trimmed = title.trim_end_matches('#');
    if trimmed.len() != title.len() && (trimmed.is_empty() || trimmed.ends_with([' ', '\t'])) {
        title = trimmed.trim_end().to_string();
    }
    Some((depth, title))
}

fn setext_level(line: &str) -> Option<usize> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let t = line[indent..].trim_end();
    if t.is_empty() {
        return None;
    }
    if t.bytes().all(|b| b == b'=') {
        Some(1)
    } else if t.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

fn is_paragraph_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.is_empty() || leading_spaces(line) > 3 {
        return false;
    }
    let first = t.as_bytes()[0];
    if matches!(first, b'>' | b'|' | b'<' | b'#') {
        return false;
    }
    // List items.
    if (first == b'-' || first == b'*' || first == b'+')
        && (t.len() == 1 || t.as_bytes()[1] == b' ')
    {
        return false;
    }
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && matches!(t.as_bytes().get(digits), Some(b'.') | Some(b')')) {
        return false;
    }
    true
}

fn is_marker_open(line: &str) -> Option<usize> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let after = rest.strip_prefix("<!--")?;
    let ws = after.len() - after.trim_start().len();
    let kw = &after[ws..];
    if !kw.starts_with(MARKER_KEYWORD) {
        return None;
    }
    let tail = &kw[MARKER_KEYWORD.len()..];
    if tail.is_empty() || tail.starts_with([' ', '\t']) || tail.starts_with("-->") {
        Some(indent + 4 + ws + MARKER_KEYWORD.len())
    } else {
        None
    }
}

/// Locate the frontmatter block (after an optional BOM and leading blank lines).
fn find_frontmatter(
    text: &str,
    lines: &[Line],
    bom: usize,
) -> Result<Option<(FrontmatterBlock, usize)>, ScanProblem> {
    let mut idx = 0;
    while idx < lines.len()
        && text[lines[idx].start..lines[idx].content_end]
            .trim()
            .is_empty()
    {
        idx += 1;
    }
    let Some(open) = lines.get(idx) else {
        return Ok(None);
    };
    let first = text[open.start..open.content_end].trim_start_matches(BOM);
    let open_start = open.start.max(bom);
    if first.trim_end() != "---" {
        return Ok(None);
    }
    for (j, l) in lines.iter().enumerate().skip(idx + 1) {
        let t = text[l.start..l.content_end].trim_end();
        if t == "---" || t == "..." {
            return Ok(Some((
                FrontmatterBlock {
                    start: open_start,
                    end: l.end,
                    inner_start: open.end,
                    inner_end: l.start,
                },
                j + 1,
            )));
        }
    }
    Err(ScanProblem {
        code: "FRONTMATTER_UNTERMINATED",
        message: "Frontmatter starts with '---' but has no closing '---' line; it is treated as body text".to_string(),
        offset: open_start,
    })
}

pub fn scan(text: &str) -> ScannedDoc {
    let mut doc = ScannedDoc {
        bom: if text.starts_with(BOM) { BOM.len() } else { 0 },
        ..Default::default()
    };
    let lines = split_lines(text);
    let mut first_body_line = 0;
    match find_frontmatter(text, &lines, doc.bom) {
        Ok(Some((fm, next))) => {
            doc.frontmatter = Some(fm);
            first_body_line = next;
        }
        Ok(None) => {}
        Err(p) => doc.problems.push(p),
    }

    let mut fence: Option<(u8, usize)> = None;
    let mut in_comment = false;
    let mut conflict_start: Option<usize> = None;
    let mut para_start: Option<usize> = None; // line index of current paragraph start
    let mut i = first_body_line;
    while i < lines.len() {
        let l = lines[i];
        let raw = &text[l.start..l.content_end];
        let line = if l.start == 0 {
            raw.trim_start_matches(BOM)
        } else {
            raw
        };

        // Merge-conflict regions.
        if let Some(cs) = conflict_start {
            if line.starts_with(">>>>>>>") && (line.len() == 7 || line.as_bytes()[7] == b' ') {
                doc.conflicts.push((cs, l.end));
                conflict_start = None;
            }
            i += 1;
            para_start = None;
            continue;
        }
        if fence.is_none()
            && !in_comment
            && line.starts_with("<<<<<<<")
            && (line.len() == 7 || line.as_bytes()[7] == b' ')
        {
            conflict_start = Some(l.start);
            doc.problems.push(ScanProblem {
                code: "MERGE_CONFLICT_MARKERS",
                message: "Unresolved merge-conflict markers; headings and entity markers inside are ignored".to_string(),
                offset: l.start,
            });
            i += 1;
            para_start = None;
            continue;
        }

        // Fenced code blocks.
        if let Some((ch, count)) = fence {
            if fence_closes(line, ch, count) {
                fence = None;
            }
            i += 1;
            continue;
        }
        if in_comment {
            if line.contains("-->") {
                in_comment = false;
            }
            i += 1;
            continue;
        }
        if let Some(f) = fence_open(line) {
            fence = Some(f);
            para_start = None;
            i += 1;
            continue;
        }

        // Entity markers.
        if let Some(kw_end) = is_marker_open(line) {
            let line_off = l.start + (raw.len() - line.len());
            let after_kw = line_off + kw_end;
            // Find the closing `-->` from after the keyword.
            match text[after_kw..].find("-->") {
                Some(rel) => {
                    let close = after_kw + rel;
                    let close_end = close + 3;
                    let first_line_end = l.content_end;
                    let (attrs, yaml_start, yaml_end) = if close <= first_line_end {
                        (text[after_kw..close].trim().to_string(), close, close)
                    } else {
                        (
                            text[after_kw..first_line_end].trim().to_string(),
                            l.end,
                            close,
                        )
                    };
                    // Extend through the closing line when only whitespace follows `-->`.
                    let mut end = close_end;
                    let mut j = i;
                    while j < lines.len() && lines[j].end <= close {
                        j += 1;
                    }
                    if j < lines.len() {
                        let cl = lines[j];
                        if text[close_end..cl.content_end].trim().is_empty() {
                            end = cl.end;
                        }
                    }
                    doc.markers.push(EntityMarker {
                        start: line_off,
                        end,
                        attrs,
                        yaml_start,
                        yaml_end,
                    });
                    // Resume at the first line after the marker.
                    i = lines
                        .iter()
                        .position(|ln| ln.start >= end)
                        .unwrap_or(lines.len());
                    para_start = None;
                    continue;
                }
                None => {
                    doc.problems.push(ScanProblem {
                        code: "WIKI_PARSE_ERROR",
                        message: "Entity marker `<!-- kb:entity` is never closed with `-->`"
                            .to_string(),
                        offset: line_off,
                    });
                    i += 1;
                    continue;
                }
            }
        }

        let trimmed = line.trim();
        // Grounding anchors (whole-line comments).
        if let Some(prefix) = ANCHOR_PREFIXES.iter().find(|p| trimmed.starts_with(**p)) {
            if let Some(inner) = trimmed[prefix.len()..].strip_suffix("-->") {
                // An optional `#<body_hash>` suffix commits the anchor's baseline.
                let (reference, body_hash) = crate::graph::grounding::split_anchor(inner);
                if !reference.is_empty() {
                    doc.anchors.push(Anchor {
                        start: l.start,
                        end: l.end,
                        reference,
                        body_hash,
                    });
                }
                para_start = None;
                i += 1;
                continue;
            }
        }
        // Other HTML comments: skip their content.
        if let Some(rest) = trimmed.strip_prefix("<!--") {
            if !rest.contains("-->") {
                in_comment = true;
            }
            para_start = None;
            i += 1;
            continue;
        }

        if let Some((depth, title)) = atx_heading(line) {
            doc.headings.push(Heading {
                depth,
                start: l.start,
                end: l.end,
                title,
            });
            para_start = None;
            i += 1;
            continue;
        }
        if let (Some(ps), Some(depth)) = (para_start, setext_level(line)) {
            let title = (ps..i)
                .map(|k| text[lines[k].start..lines[k].content_end].trim())
                .collect::<Vec<_>>()
                .join(" ");
            doc.headings.push(Heading {
                depth,
                start: lines[ps].start,
                end: l.end,
                title,
            });
            para_start = None;
            i += 1;
            continue;
        }
        if is_paragraph_line(line) {
            if para_start.is_none() {
                para_start = Some(i);
            }
        } else {
            para_start = None;
        }
        i += 1;
    }
    if let Some(cs) = conflict_start {
        doc.conflicts.push((cs, text.len()));
    }
    doc
}

/// Replace `[start, end)` edits in `original` (sorted, non-overlapping), verifying that text
/// outside the declared ranges is preserved byte for byte.
#[derive(Debug, Clone)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub label: String,
}

pub fn apply_edits(original: &str, edits: &[Edit]) -> Result<String, String> {
    let mut ordered: Vec<&Edit> = edits.iter().collect();
    ordered.sort_by_key(|e| (e.start, e.end));
    for e in &ordered {
        if e.end < e.start {
            return Err(format!("{} [{}, {}) is inverted", e.label, e.start, e.end));
        }
        if e.end > original.len() {
            return Err(format!(
                "{} [{}, {}) lies outside the text (length {})",
                e.label,
                e.start,
                e.end,
                original.len()
            ));
        }
        if !original.is_char_boundary(e.start) || !original.is_char_boundary(e.end) {
            return Err(format!("{} does not fall on a character boundary", e.label));
        }
    }
    for w in ordered.windows(2) {
        if w[1].start < w[0].end {
            return Err(format!("Edits overlap: {} and {}", w[0].label, w[1].label));
        }
    }
    let mut out = String::with_capacity(original.len());
    let mut cursor = 0;
    for e in &ordered {
        out.push_str(&original[cursor..e.start]);
        out.push_str(&e.text);
        cursor = e.end;
    }
    out.push_str(&original[cursor..]);
    // Scope check: every untouched segment survives, in order.
    let mut search = 0;
    let mut cursor = 0;
    for e in &ordered {
        let seg = &original[cursor..e.start];
        if !seg.is_empty() {
            match out[search..].find(seg) {
                Some(p) => search += p + seg.len(),
                None => return Err(format!("Text before {} changed", e.label)),
            }
        }
        cursor = e.end;
    }
    if !original[cursor..].is_empty() && !out.ends_with(&original[cursor..]) {
        return Err("Text after the last edit changed".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_headings_markers_and_fences() {
        let text = "---\nid: kb_a\n---\n# Title\n\nintro\n\n```md\n# not a heading\n<!-- kb:entity id=x -->\n```\n\n<!-- kb:entity id=kb_b type=decision -->\n## Decision B\n\nbody\n\nSetext\n---\n";
        let doc = scan(text);
        assert!(doc.frontmatter.is_some());
        let titles: Vec<&str> = doc.headings.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, vec!["Title", "Decision B", "Setext"]);
        assert_eq!(doc.markers.len(), 1);
        assert_eq!(doc.markers[0].attrs, "id=kb_b type=decision");
    }

    #[test]
    fn crlf_and_bom() {
        let text = "\u{feff}---\r\nid: kb_a\r\n---\r\n# T\r\n<!-- kb:entity\r\nid: kb_c\r\n-->\r\n## C\r\n";
        let doc = scan(text);
        let fm = doc.frontmatter.unwrap();
        assert_eq!(&text[fm.inner_start..fm.inner_end], "id: kb_a\r\n");
        assert_eq!(doc.markers.len(), 1);
        let m = &doc.markers[0];
        assert_eq!(&text[m.yaml_start..m.yaml_end], "id: kb_c\r\n");
        assert_eq!(doc.headings.len(), 2);
        assert_eq!(doc.headings[1].title, "C");
    }

    #[test]
    fn edits_preserve_outside_text() {
        let out = apply_edits(
            "abcdef",
            &[Edit {
                start: 2,
                end: 4,
                text: "XY".into(),
                label: "t".into(),
            }],
        )
        .unwrap();
        assert_eq!(out, "abXYef");
    }
}
