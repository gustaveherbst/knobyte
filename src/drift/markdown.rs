//! A small, line-oriented markdown reader covering what the drift checkers need: YAML
//! frontmatter, top-level ATX/setext headings, fenced code blocks, HTML comments, paragraphs
//! (including list-item paragraphs) and the inline code spans and leading bold of list items.
//!
//! It follows CommonMark closely enough for claim extraction; it is not a renderer.

use std::sync::OnceLock;

use regex::Regex;

/// An inline code span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineCode {
    pub value: String,
    /// 1-based line the span starts on.
    pub line: usize,
    /// Paragraph the span belongs to (headings have none).
    pub paragraph: Option<usize>,
    /// Character offset of the span in its paragraph's plain text.
    pub offset: usize,
    /// The span is inside the leading bold of a list item.
    pub in_lead_strong: bool,
}

/// A fenced code block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    /// 1-based line of the opening fence.
    pub line: usize,
    pub lines: Vec<String>,
}

/// Bold text that opens a list item (`- **name** — description`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadStrong {
    pub line: usize,
    /// Concatenated plain-text children (inline code excluded), trimmed.
    pub text: String,
    /// Values of inline code spans inside the bold.
    pub codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    pub line: usize,
    /// Plain text in reading order (inline code values included, markup removed).
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct MarkdownDoc {
    /// Top-level headings as (1-based line, text).
    pub headings: Vec<(usize, String)>,
    pub inline_codes: Vec<InlineCode>,
    pub code_blocks: Vec<CodeBlock>,
    pub lead_strongs: Vec<LeadStrong>,
    pub paragraphs: Vec<Paragraph>,
    /// Raw YAML frontmatter, when present.
    pub frontmatter: Option<String>,
}

impl MarkdownDoc {
    /// Text of the last top-level heading at or before `line`.
    pub fn heading_at_line(&self, line: usize) -> Option<&str> {
        self.headings
            .iter()
            .take_while(|(l, _)| *l <= line)
            .last()
            .map(|(_, t)| t.as_str())
    }
}

/// Split `content` into its YAML frontmatter (if any) and the 1-based line where the body starts.
pub fn split_frontmatter(content: &str) -> (Option<String>, usize) {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.first().map(|l| l.trim_end_matches('\r')) != Some("---") {
        return (None, 1);
    }
    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim_end_matches('\r') == "---" {
            let yaml = lines[1..i]
                .iter()
                .map(|l| l.trim_end_matches('\r'))
                .collect::<Vec<_>>()
                .join("\n");
            return (Some(yaml), i + 2);
        }
    }
    (None, 1)
}

/// Parsed YAML frontmatter as a JSON value (`None` when absent or invalid).
pub fn parse_frontmatter(content: &str) -> Option<serde_json::Value> {
    let (yaml, _) = split_frontmatter(content);
    let yaml = yaml?;
    let value: serde_yaml::Value = serde_yaml::from_str(&yaml).ok()?;
    serde_json::to_value(value).ok()
}

/// The 1-based line (within `yaml`) a YAML error points at. serde_yaml reports a duplicate
/// key at the start of its mapping, so for those the line of the repeated key is found
/// instead (the second occurrence at the shallowest indentation).
pub fn yaml_error_line(yaml: &str, err: &serde_yaml::Error) -> Option<usize> {
    let msg = err.to_string();
    if let Some(key) = msg
        .split_once("duplicate entry with key \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(k, _)| k.to_string())
    {
        let mut hits: Vec<(usize, usize)> = Vec::new(); // (indent, line)
        for (i, line) in yaml.lines().enumerate() {
            let t = line.trim_start();
            let rest = t.strip_prefix("- ").unwrap_or(t);
            let rest = rest.trim_start_matches(['"', '\'']);
            if rest.strip_prefix(key.as_str()).is_some_and(|r| r.trim_start_matches(['"', '\'']).trim_start().starts_with(':')) {
                hits.push((line.len() - t.len(), i + 1));
            }
        }
        if let Some(min) = hits.iter().map(|(ind, _)| *ind).min() {
            if let Some((_, line)) = hits.iter().filter(|(ind, _)| *ind == min).nth(1) {
                return Some(*line);
            }
        }
    }
    err.location().map(|l| l.line())
}

/// Why the frontmatter of `content` is unusable, when it is: `(code, message, 1-based line)`
/// with code `FRONTMATTER_PARSE_ERROR` (invalid YAML, e.g. a duplicate key, or a non-map)
/// or `FRONTMATTER_UNTERMINATED` (no closing `---`). Mirrors the wiki parser's diagnostics.
pub fn frontmatter_problem(content: &str) -> Option<(&'static str, String, usize)> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let (yaml, _) = split_frontmatter(content);
    let Some(yaml) = yaml else {
        let opens = content.split('\n').next().map(|l| l.trim_end_matches('\r')) == Some("---");
        return opens.then(|| {
            ("FRONTMATTER_UNTERMINATED", "Frontmatter opened with `---` is never closed (fields ignored)".to_string(), 1)
        });
    };
    if yaml.trim().is_empty() {
        return None;
    }
    match serde_yaml::from_str::<serde_yaml::Value>(&yaml) {
        Err(e) => Some((
            "FRONTMATTER_PARSE_ERROR",
            format!("Invalid YAML frontmatter (fields ignored): {}", e),
            1 + yaml_error_line(&yaml, &e).unwrap_or(0),
        )),
        Ok(serde_yaml::Value::Mapping(_)) | Ok(serde_yaml::Value::Null) => None,
        Ok(_) => Some((
            "FRONTMATTER_PARSE_ERROR",
            "Invalid YAML frontmatter (fields ignored): metadata is not a key/value map".to_string(),
            1,
        )),
    }
}

fn list_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s*)([-*+]|\d{1,9}[.)])(\s+|$)(.*)$").unwrap())
}

fn thematic_break_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s{0,3}([-*_])(\s*([-*_]))+\s*$").unwrap())
}

fn atx_heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s{0,3}(#{1,6})(?:\s+(.*?))?\s*$").unwrap())
}

fn is_thematic_break(line: &str) -> bool {
    if !thematic_break_re().is_match(line) {
        return false;
    }
    let chars: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3 && chars.iter().all(|c| *c == chars[0])
}

fn fence_open(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|c| *c == first).count();
    if len < 3 {
        return None;
    }
    // A backtick fence's info string cannot contain backticks.
    if first == '`' && trimmed[len..].contains('`') {
        return None;
    }
    Some((first, len))
}

fn is_fence_close(line: &str, ch: char, len: usize) -> bool {
    let trimmed = line.trim();
    let n = trimmed.chars().take_while(|c| *c == ch).count();
    n >= len && trimmed.chars().all(|c| c == ch)
}

#[derive(Default)]
struct ParaBuilder {
    start_line: usize,
    /// (line number, text) pairs.
    lines: Vec<(usize, String)>,
    /// First line is a list item content line.
    list_item: bool,
}

/// Parse the markdown body of `content` (frontmatter excluded; line numbers stay absolute).
pub fn parse(content: &str) -> MarkdownDoc {
    let (frontmatter, body_start) = split_frontmatter(content);
    let mut doc = MarkdownDoc {
        frontmatter,
        ..Default::default()
    };

    let all_lines: Vec<&str> = content.split('\n').collect();
    let mut para: Option<ParaBuilder> = None;
    let mut fence: Option<(char, usize, CodeBlock)> = None;
    let mut in_comment = false;

    let flush = |para: &mut Option<ParaBuilder>, doc: &mut MarkdownDoc| {
        if let Some(p) = para.take() {
            finish_paragraph(p, doc);
        }
    };

    for (idx, raw) in all_lines.iter().enumerate().skip(body_start - 1) {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r');

        if let Some((ch, len, block)) = fence.as_mut() {
            if is_fence_close(line, *ch, *len) {
                let (_, _, block) = fence.take().unwrap();
                doc.code_blocks.push(block);
            } else {
                block.lines.push(line.to_string());
            }
            continue;
        }

        if in_comment {
            if line.contains("-->") {
                in_comment = false;
            }
            continue;
        }

        // Blockquote markers are stripped; their content is not top-level.
        let mut top_level = true;
        let mut text = line;
        while let Some(rest) = text.trim_start().strip_prefix('>') {
            top_level = false;
            text = rest.strip_prefix(' ').unwrap_or(rest);
        }

        if text.trim().is_empty() {
            flush(&mut para, &mut doc);
            continue;
        }

        if let Some((ch, len)) = fence_open(text) {
            flush(&mut para, &mut doc);
            fence = Some((
                ch,
                len,
                CodeBlock {
                    line: line_no,
                    lines: Vec::new(),
                },
            ));
            continue;
        }

        let trimmed = text.trim_start();
        if trimmed.starts_with("<!--") && (text.len() - trimmed.len()) < 4 {
            flush(&mut para, &mut doc);
            if !trimmed[4..].contains("-->") {
                in_comment = true;
            }
            continue;
        }

        if let Some(caps) = atx_heading_re().captures(text) {
            flush(&mut para, &mut doc);
            let raw_text = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let raw_text = strip_closing_hashes(raw_text);
            add_heading(&mut doc, line_no, raw_text, top_level);
            continue;
        }

        // Setext underline turns the open paragraph into a heading.
        if let Some(p) = para.as_ref() {
            let t = text.trim();
            let setext = !t.is_empty()
                && (t.chars().all(|c| c == '=') || t.chars().all(|c| c == '-'))
                && (text.len() - trimmed.len()) < 4
                && !p.list_item;
            if setext {
                let p = para.take().unwrap();
                let joined = p
                    .lines
                    .iter()
                    .map(|(_, l)| l.trim())
                    .collect::<Vec<_>>()
                    .join("\n");
                add_heading(&mut doc, p.start_line, &joined, top_level);
                continue;
            }
        }

        if is_thematic_break(text) {
            flush(&mut para, &mut doc);
            continue;
        }

        if let Some(caps) = list_item_re().captures(text) {
            flush(&mut para, &mut doc);
            let content = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            if content.trim().is_empty() {
                continue;
            }
            if let Some(c) = atx_heading_re().captures(content) {
                let raw_text = c.get(2).map(|m| m.as_str()).unwrap_or("");
                add_heading(&mut doc, line_no, strip_closing_hashes(raw_text), false);
                continue;
            }
            para = Some(ParaBuilder {
                start_line: line_no,
                lines: vec![(line_no, content.to_string())],
                list_item: true,
            });
            continue;
        }

        match para.as_mut() {
            Some(p) => p.lines.push((line_no, trimmed.to_string())),
            None => {
                para = Some(ParaBuilder {
                    start_line: line_no,
                    lines: vec![(line_no, trimmed.to_string())],
                    list_item: false,
                })
            }
        }
    }

    flush(&mut para, &mut doc);
    if let Some((_, _, block)) = fence.take() {
        // Unterminated fence runs to the end of the document.
        doc.code_blocks.push(block);
    }
    doc
}

fn strip_closing_hashes(s: &str) -> &str {
    let t = s.trim_end();
    let without = t.trim_end_matches('#');
    if without.len() != t.len() && (without.is_empty() || without.ends_with(' ')) {
        without.trim_end()
    } else {
        t
    }
}

fn add_heading(doc: &mut MarkdownDoc, line_no: usize, raw: &str, top_level: bool) {
    let inline = parse_inline(raw, &[(line_no, raw.len())]);
    for code in &inline.codes {
        doc.inline_codes.push(InlineCode {
            value: code.value.clone(),
            line: code.line,
            paragraph: None,
            offset: code.offset,
            in_lead_strong: false,
        });
    }
    if top_level {
        doc.headings.push((line_no, inline.text));
    }
}

fn finish_paragraph(p: ParaBuilder, doc: &mut MarkdownDoc) {
    let joined = p
        .lines
        .iter()
        .map(|(_, l)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let spans: Vec<(usize, usize)> = p.lines.iter().map(|(n, l)| (*n, l.len())).collect();
    let inline = parse_inline(&joined, &spans);
    let para_idx = doc.paragraphs.len();
    let lead_codes: Vec<usize> = if p.list_item {
        inline.lead_strong_codes.clone()
    } else {
        Vec::new()
    };
    for (i, code) in inline.codes.iter().enumerate() {
        doc.inline_codes.push(InlineCode {
            value: code.value.clone(),
            line: code.line,
            paragraph: Some(para_idx),
            offset: code.offset,
            in_lead_strong: lead_codes.contains(&i),
        });
    }
    if p.list_item {
        if let Some(text) = inline.lead_strong_text {
            doc.lead_strongs.push(LeadStrong {
                line: p.start_line,
                text,
                codes: lead_codes
                    .iter()
                    .map(|i| inline.codes[*i].value.clone())
                    .collect(),
            });
        }
    }
    doc.paragraphs.push(Paragraph {
        line: p.start_line,
        text: inline.text,
    });
}

struct InlineSpan {
    value: String,
    line: usize,
    offset: usize,
}

struct InlineResult {
    text: String,
    codes: Vec<InlineSpan>,
    /// Plain text of a strong span that opens the content (trimmed), if any.
    lead_strong_text: Option<String>,
    /// Indexes into `codes` of spans inside the leading strong.
    lead_strong_codes: Vec<usize>,
}

/// Line number of byte offset `pos` in a text joined from `spans` with `\n`.
fn line_of(spans: &[(usize, usize)], pos: usize) -> usize {
    let mut start = 0;
    for (line, len) in spans {
        if pos <= start + len {
            return *line;
        }
        start += len + 1;
    }
    spans.last().map(|(l, _)| *l).unwrap_or(1)
}

/// Inline pass: code spans, escapes, HTML comments, link destinations and emphasis markers.
fn parse_inline(src: &str, spans: &[(usize, usize)]) -> InlineResult {
    let bytes = src.as_bytes();
    let mut text = String::new();
    let mut codes: Vec<InlineSpan> = Vec::new();
    let mut i = 0;

    // Leading strong tracking.
    let lead_marker = if src.starts_with("**") {
        Some("**")
    } else if src.starts_with("__") {
        Some("__")
    } else {
        None
    };
    let mut lead_open = lead_marker.is_some();
    let mut lead_text = String::new();
    let mut lead_codes: Vec<usize> = Vec::new();
    let mut lead_closed = false;
    if lead_marker.is_some() {
        i = 2;
    }

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() {
            let ch = bytes[i + 1] as char;
            text.push(ch);
            if lead_open {
                lead_text.push(ch);
            }
            i += 2;
            continue;
        }
        if c == b'`' {
            let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
            // Find a closing run of exactly the same length.
            let mut j = i + run;
            let mut close = None;
            while j < bytes.len() {
                if bytes[j] == b'`' {
                    let r = bytes[j..].iter().take_while(|b| **b == b'`').count();
                    if r == run {
                        close = Some(j);
                        break;
                    }
                    j += r;
                } else {
                    j += 1;
                }
            }
            match close {
                Some(end) => {
                    let mut value = src[i + run..end].replace('\n', " ");
                    if value.len() >= 2
                        && value.starts_with(' ')
                        && value.ends_with(' ')
                        && !value.trim().is_empty()
                    {
                        value = value[1..value.len() - 1].to_string();
                    }
                    let offset = text.chars().count();
                    text.push_str(&value);
                    if lead_open {
                        lead_codes.push(codes.len());
                    }
                    codes.push(InlineSpan {
                        value,
                        line: line_of(spans, i),
                        offset,
                    });
                    i = end + run;
                }
                None => {
                    // Unmatched backticks are literal text.
                    let lit = &src[i..i + run];
                    text.push_str(lit);
                    if lead_open {
                        lead_text.push_str(lit);
                    }
                    i += run;
                }
            }
            continue;
        }
        if src[i..].starts_with("<!--") {
            match src[i + 4..].find("-->") {
                Some(end) => {
                    i = i + 4 + end + 3;
                    continue;
                }
                None => break,
            }
        }
        if let Some(marker) = lead_marker {
            if lead_open && src[i..].starts_with(marker) {
                lead_open = false;
                lead_closed = true;
                i += 2;
                continue;
            }
        }
        if c == b']' && src[i..].starts_with("](") {
            // Drop the link destination.
            if let Some(end) = src[i + 2..].find(')') {
                i = i + 2 + end + 1;
                continue;
            }
        }
        if c == b'*' || c == b'[' || c == b']' {
            i += 1;
            continue;
        }
        let ch = src[i..].chars().next().unwrap();
        text.push(ch);
        if lead_open {
            lead_text.push(ch);
        }
        i += ch.len_utf8();
    }

    let (lead_strong_text, lead_strong_codes) = if lead_closed {
        (Some(lead_text.trim().to_string()), lead_codes)
    } else {
        (None, Vec::new())
    };

    InlineResult {
        text,
        codes,
        lead_strong_text,
        lead_strong_codes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_codes_and_blocks() {
        let md = "---\nname: x\n---\n# Setup\n\nRun `npm run build` now.\n\n```sh\nnpm install\n```\n\n## Key Libraries\n\n- **Express** — web\n- **`dagre` + `elk`** — layout\n";
        let doc = parse(md);
        assert_eq!(doc.frontmatter.as_deref(), Some("name: x"));
        assert_eq!(
            doc.headings,
            vec![(4, "Setup".to_string()), (12, "Key Libraries".to_string())]
        );
        assert_eq!(doc.inline_codes[0].value, "npm run build");
        assert_eq!(doc.inline_codes[0].line, 6);
        assert_eq!(doc.code_blocks[0].line, 8);
        assert_eq!(doc.code_blocks[0].lines, vec!["npm install"]);
        assert_eq!(doc.lead_strongs.len(), 2);
        assert_eq!(doc.lead_strongs[0].text, "Express");
        assert_eq!(doc.lead_strongs[1].codes, vec!["dagre", "elk"]);
        assert_eq!(doc.heading_at_line(15), Some("Key Libraries"));
    }

    #[test]
    fn skips_comments_and_reads_setext() {
        let md = "Title\n=====\n\n<!--\n`hidden.ts`\n-->\nText `shown.ts` <!-- `x.ts` --> end\n";
        let doc = parse(md);
        assert_eq!(doc.headings, vec![(1, "Title".to_string())]);
        let values: Vec<_> = doc.inline_codes.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["shown.ts"]);
    }
}
