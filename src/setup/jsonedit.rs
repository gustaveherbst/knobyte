//! Minimal, formatting-preserving JSON edits for configuration files Knobyte shares with other
//! tools (`.mcp.json`, `.cursor/mcp.json`, `.vscode/mcp.json`, `opencode.json`, ...).
//!
//! The document is parsed into a tree of byte spans; an edit splices new text into the
//! original instead of re-serializing it, so key order, indentation, comments (`//`, `/* */`,
//! as JSONC allows) and every unrelated byte survive. Trailing commas are accepted.

use serde_json::Value;

/// An ordered JSON value for rendering (Knobyte's own entries keep a readable key order).
#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Str(String),
    Bool(bool),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn obj(members: Vec<(&str, J)>) -> J {
        J::Obj(members.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    pub fn strs<S: AsRef<str>>(items: &[S]) -> J {
        J::Arr(items.iter().map(|s| J::Str(s.as_ref().to_string())).collect())
    }
    pub fn to_value(&self) -> Value {
        match self {
            J::Str(s) => Value::String(s.clone()),
            J::Bool(b) => Value::Bool(*b),
            J::Arr(a) => Value::Array(a.iter().map(J::to_value).collect()),
            J::Obj(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_value())).collect()),
        }
    }
    fn is_scalar(&self) -> bool {
        matches!(self, J::Str(_) | J::Bool(_))
    }

    /// Render with `unit` indentation, continuation lines prefixed by `base`.
    pub fn render(&self, unit: &str, base: &str, eol: &str) -> String {
        match self {
            J::Str(s) => serde_json::to_string(s).unwrap_or_default(),
            J::Bool(b) => b.to_string(),
            J::Arr(items) if items.iter().all(J::is_scalar) => {
                format!("[{}]", items.iter().map(|i| i.render(unit, base, eol)).collect::<Vec<_>>().join(", "))
            }
            J::Arr(items) => {
                let inner = format!("{}{}", base, unit);
                let parts: Vec<String> =
                    items.iter().map(|i| format!("{}{}", inner, i.render(unit, &inner, eol))).collect();
                format!("[{}{}{}{}]", eol, parts.join(&format!(",{}", eol)), eol, base)
            }
            J::Obj(m) if m.is_empty() => "{}".to_string(),
            J::Obj(m) => {
                let inner = format!("{}{}", base, unit);
                let parts: Vec<String> = m
                    .iter()
                    .map(|(k, v)| {
                        format!("{}{}: {}", inner, serde_json::to_string(k).unwrap_or_default(), v.render(unit, &inner, eol))
                    })
                    .collect();
                format!("{{{}{}{}{}}}", eol, parts.join(&format!(",{}", eol)), eol, base)
            }
        }
    }

    /// A new document holding this value (two-space indentation, trailing newline).
    pub fn document(&self) -> String {
        format!("{}\n", self.render("  ", "", "\n"))
    }
}

#[derive(Debug, Clone)]
pub enum Node {
    Object { open: usize, close: usize, members: Vec<Member>, trailing_comma: Option<usize> },
    Array { open: usize, close: usize, items: Vec<Node>, trailing_comma: Option<usize> },
    Scalar { start: usize, end: usize },
}

#[derive(Debug, Clone)]
pub struct Member {
    pub key: String,
    pub key_start: usize,
    pub value: Node,
}

impl Node {
    pub fn start(&self) -> usize {
        match self {
            Node::Object { open, .. } | Node::Array { open, .. } => *open,
            Node::Scalar { start, .. } => *start,
        }
    }
    /// Exclusive end.
    pub fn end(&self) -> usize {
        match self {
            Node::Object { close, .. } | Node::Array { close, .. } => close + 1,
            Node::Scalar { end, .. } => *end,
        }
    }
    pub fn member(&self, key: &str) -> Option<&Member> {
        match self {
            Node::Object { members, .. } => members.iter().find(|m| m.key == key),
            _ => None,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, what: &str) -> Result<T, String> {
        let line = self.text[..self.i.min(self.text.len())].matches('\n').count() + 1;
        Err(format!("{} at line {}", what, line))
    }

    fn skip_ws(&mut self) -> Result<(), String> {
        loop {
            match self.s.get(self.i) {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.i += 1,
                Some(b'/') if self.s.get(self.i + 1) == Some(&b'/') => {
                    while self.i < self.s.len() && self.s[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                Some(b'/') if self.s.get(self.i + 1) == Some(&b'*') => {
                    let Some(end) = self.text[self.i + 2..].find("*/") else {
                        return self.err("unterminated comment");
                    };
                    self.i += 2 + end + 2;
                }
                _ => return Ok(()),
            }
        }
    }

    fn string(&mut self) -> Result<(usize, usize), String> {
        let start = self.i;
        self.i += 1;
        while let Some(&c) = self.s.get(self.i) {
            match c {
                b'\\' => self.i += 2,
                b'"' => {
                    self.i += 1;
                    return Ok((start, self.i));
                }
                _ => self.i += 1,
            }
        }
        self.err("unterminated string")
    }

    fn value(&mut self, depth: usize) -> Result<Node, String> {
        if depth > 64 {
            return self.err("nesting too deep");
        }
        self.skip_ws()?;
        match self.s.get(self.i) {
            Some(b'{') => {
                let open = self.i;
                self.i += 1;
                let mut members = Vec::new();
                let mut trailing_comma = None;
                loop {
                    self.skip_ws()?;
                    match self.s.get(self.i) {
                        Some(b'}') => {
                            return Ok(Node::Object { open, close: self.i, members, trailing_comma }.advance(self));
                        }
                        Some(b'"') if trailing_comma.is_some() || members.is_empty() => {
                            trailing_comma = None;
                            let (ks, ke) = self.string()?;
                            let key: String = serde_json::from_str(&self.text[ks..ke])
                                .map_err(|e| format!("invalid key: {}", e))?;
                            self.skip_ws()?;
                            if self.s.get(self.i) != Some(&b':') {
                                return self.err("expected ':'");
                            }
                            self.i += 1;
                            let value = self.value(depth + 1)?;
                            members.push(Member { key, key_start: ks, value });
                            self.skip_ws()?;
                            match self.s.get(self.i) {
                                Some(b',') => {
                                    trailing_comma = Some(self.i);
                                    self.i += 1;
                                }
                                Some(b'}') => {}
                                _ => return self.err("expected ',' or '}'"),
                            }
                        }
                        _ => return self.err("expected a key or '}'"),
                    }
                }
            }
            Some(b'[') => {
                let open = self.i;
                self.i += 1;
                let mut items = Vec::new();
                let mut trailing_comma = None;
                loop {
                    self.skip_ws()?;
                    if self.s.get(self.i) == Some(&b']') {
                        return Ok(Node::Array { open, close: self.i, items, trailing_comma }.advance(self));
                    }
                    if !items.is_empty() && trailing_comma.is_none() {
                        return self.err("expected ',' or ']'");
                    }
                    trailing_comma = None;
                    items.push(self.value(depth + 1)?);
                    self.skip_ws()?;
                    match self.s.get(self.i) {
                        Some(b',') => {
                            trailing_comma = Some(self.i);
                            self.i += 1;
                        }
                        Some(b']') => {}
                        _ => return self.err("expected ',' or ']'"),
                    }
                }
            }
            Some(b'"') => {
                let (start, end) = self.string()?;
                serde_json::from_str::<String>(&self.text[start..end]).map_err(|e| format!("invalid string: {}", e))?;
                Ok(Node::Scalar { start, end })
            }
            Some(_) => {
                let start = self.i;
                while let Some(&c) = self.s.get(self.i) {
                    if matches!(c, b',' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n' | b'/') {
                        break;
                    }
                    self.i += 1;
                }
                if serde_json::from_str::<Value>(&self.text[start..self.i]).is_err() || start == self.i {
                    return self.err("invalid value");
                }
                Ok(Node::Scalar { start, end: self.i })
            }
            None => self.err("unexpected end of input"),
        }
    }
}

impl Node {
    fn advance(self, p: &mut Parser) -> Node {
        p.i += 1;
        self
    }
}

/// Parse a JSON (or JSONC) document into spans.
pub fn parse(text: &str) -> Result<Node, String> {
    let mut p = Parser { s: text.as_bytes(), text, i: 0 };
    // A UTF-8 byte order mark is tolerated.
    if text.starts_with('\u{feff}') {
        p.i = 3;
    }
    let root = p.value(0)?;
    p.skip_ws()?;
    if p.i != text.len() {
        return p.err("unexpected trailing content");
    }
    Ok(root)
}

/// The value at `node` as plain JSON (comments removed).
pub fn node_value(text: &str, node: &Node) -> Option<Value> {
    match node {
        Node::Scalar { start, end } => serde_json::from_str(&text[*start..*end]).ok(),
        Node::Array { items, .. } => Some(Value::Array(items.iter().map(|i| node_value(text, i)).collect::<Option<Vec<_>>>()?)),
        Node::Object { members, .. } => Some(Value::Object(
            members
                .iter()
                .map(|m| node_value(text, &m.value).map(|v| (m.key.clone(), v)))
                .collect::<Option<serde_json::Map<_, _>>>()?,
        )),
    }
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn line_indent(text: &str, pos: usize) -> String {
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text[line_start..].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

/// Whether only whitespace precedes `pos` on its line.
fn starts_line(text: &str, pos: usize) -> bool {
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text[line_start..pos].chars().all(|c| c == ' ' || c == '\t')
}

fn indent_unit(text: &str) -> String {
    for line in text.lines() {
        let ws: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if !ws.is_empty() && line.len() > ws.len() {
            return if ws.starts_with('\t') { "\t".into() } else { ws };
        }
    }
    "  ".into()
}

/// Insert `key: value` into the object `obj` of `text`.
fn insert_member(text: &str, obj: &Node, key: &str, value: &J) -> String {
    let Node::Object { open, close, members, trailing_comma } = obj else { return text.to_string() };
    let eol = eol_of(text);
    let unit = indent_unit(text);
    let parent = line_indent(text, *open);
    let indent = match members.last() {
        Some(m) if starts_line(text, m.key_start) => line_indent(text, m.key_start),
        _ => format!("{}{}", parent, unit),
    };
    let entry = format!("{}: {}", serde_json::to_string(key).unwrap_or_default(), value.render(&unit, &indent, eol));
    match (members.last(), trailing_comma) {
        (Some(_), Some(comma)) => {
            let at = comma + 1;
            format!("{}{}{}{},{}", &text[..at], eol, indent, entry, &text[at..])
        }
        (Some(last), None) => {
            let at = last.value.end();
            format!("{},{}{}{}{}", &text[..at], eol, indent, entry, &text[at..])
        }
        (None, _) => {
            let mut at = *close;
            while at > open + 1 && matches!(text.as_bytes()[at - 1], b' ' | b'\t' | b'\r' | b'\n') {
                at -= 1;
            }
            format!("{}{}{}{}{}{}{}", &text[..at], eol, indent, entry, eol, parent, &text[*close..])
        }
    }
}

fn wrap(path: &[&str], value: &J) -> J {
    path.iter().rev().fold(value.clone(), |acc, k| J::Obj(vec![(k.to_string(), acc)]))
}

/// Set `path` (object keys from the root) to `value`, creating missing parent objects and
/// replacing only the final value's bytes. Errors when the root or an intermediate value is
/// not an object.
pub fn upsert(text: &str, path: &[&str], value: &J) -> Result<String, String> {
    let root = parse(text)?;
    let mut node = &root;
    if !matches!(node, Node::Object { .. }) {
        return Err("the top level is not a JSON object".into());
    }
    for (i, key) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        match node.member(key) {
            None => return Ok(insert_member(text, node, key, &wrap(&path[i + 1..], value))),
            Some(m) if last => {
                let eol = eol_of(text);
                let unit = indent_unit(text);
                let base = line_indent(text, m.key_start);
                let rendered = value.render(&unit, &base, eol);
                return Ok(format!("{}{}{}", &text[..m.value.start()], rendered, &text[m.value.end()..]));
            }
            Some(m) => {
                if !matches!(m.value, Node::Object { .. }) {
                    return Err(format!("`{}` is not a JSON object", path[..=i].join(".")));
                }
                node = &m.value;
            }
        }
    }
    Err("empty path".into())
}

/// The value at `path`, if present.
pub fn get(text: &str, path: &[&str]) -> Result<Option<Value>, String> {
    let root = parse(text)?;
    let mut node = &root;
    for key in path {
        match node.member(key) {
            Some(m) => node = &m.value,
            None => return Ok(None),
        }
    }
    Ok(node_value(text, node))
}

/// Append a string to the array at `path` (created when missing).
pub fn append_string(text: &str, path: &[&str], item: &str) -> Result<String, String> {
    let root = parse(text)?;
    let mut node = &root;
    for key in path {
        match node.member(key) {
            Some(m) => node = &m.value,
            None => return upsert(text, path, &J::strs(&[item])),
        }
    }
    let Node::Array { open, close, items, trailing_comma } = node else {
        return Err(format!("`{}` is not a JSON array", path.join(".")));
    };
    let quoted = serde_json::to_string(item).unwrap_or_default();
    Ok(match (items.last(), trailing_comma) {
        (None, _) => format!("{}{}{}", &text[..open + 1], quoted, &text[*close..]),
        (Some(_), Some(c)) => format!("{} {},{}", &text[..c + 1], quoted, &text[c + 1..]),
        (Some(last), None) => format!("{}, {}{}", &text[..last.end()], quoted, &text[last.end()..]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_preserves_order_comments_and_indentation() {
        let src = "{\n    // servers\n    \"mcpServers\": {\n        \"zeta\": { \"command\": \"z\" }\n    },\n    \"alpha\": 1\n}\n";
        let out = upsert(src, &["mcpServers", "knobyte"], &J::obj(vec![("command", J::Str("knobyte".into()))])).unwrap();
        assert_eq!(
            out,
            "{\n    // servers\n    \"mcpServers\": {\n        \"zeta\": { \"command\": \"z\" },\n        \"knobyte\": {\n            \"command\": \"knobyte\"\n        }\n    },\n    \"alpha\": 1\n}\n"
        );
        // Replacing touches only the value.
        let again = upsert(&out, &["mcpServers", "knobyte", "command"], &J::Str("kb".into())).unwrap();
        assert!(again.contains("\"command\": \"kb\"") && again.contains("\"zeta\": { \"command\": \"z\" }"));
        assert_eq!(get(&again, &["mcpServers", "knobyte", "command"]).unwrap(), Some(Value::String("kb".into())));
    }

    #[test]
    fn creates_containers_and_handles_trailing_commas_and_crlf() {
        let out = upsert("{}", &["servers", "knobyte"], &J::obj(vec![("type", J::Str("stdio".into()))])).unwrap();
        assert_eq!(out, "{\n  \"servers\": {\n    \"knobyte\": {\n      \"type\": \"stdio\"\n    }\n  }\n}");
        let out = upsert("{\r\n  \"a\": [1, 2,],\r\n}\r\n", &["b"], &J::Bool(true)).unwrap();
        assert_eq!(out, "{\r\n  \"a\": [1, 2,],\r\n  \"b\": true,\r\n}\r\n");
        assert!(parse(&out).is_ok());
    }

    #[test]
    fn rejects_garbage_and_non_objects() {
        assert!(parse("{\"a\": }").is_err());
        assert!(parse("{\"a\": 1} x").is_err());
        assert!(parse("{\"a\": tru}").is_err());
        assert!(upsert("[1]", &["a"], &J::Bool(true)).is_err());
        assert!(upsert("{\"mcp\": 3}", &["mcp", "knobyte"], &J::Bool(true)).is_err());
    }

    #[test]
    fn append_to_arrays() {
        assert_eq!(append_string("{\"i\": []}", &["i"], "x").unwrap(), "{\"i\": [\"x\"]}");
        assert_eq!(append_string("{\"i\": [\"a\"]}", &["i"], "x").unwrap(), "{\"i\": [\"a\", \"x\"]}");
        assert_eq!(append_string("{\"m\": 1}", &["i"], "x").unwrap(), "{\"m\": 1,\n  \"i\": [\"x\"]}");
        assert!(append_string("{\"i\": 3}", &["i"], "x").is_err());
    }
}
