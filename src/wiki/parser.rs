//! Markdown → entities.
//!
//! A file yields one file-level entity (its frontmatter, or an implicit entity derived from the
//! path when there is none) plus one section entity per `<!-- kb:entity ... -->` marker bound to
//! the heading that follows it:
//!
//! ```markdown
//! ---
//! id: kb_auth
//! title: Authentication
//! type: architecture
//! ---
//! # Authentication
//!
//! <!-- kb:entity id=kb_token_ttl type=decision status=promoted -->
//! ## Token lifetime
//!
//! Tokens live 15 minutes.
//!
//! <!-- kb:entity
//! id: kb_refresh
//! type: component
//! implements: [kb_token_ttl]
//! -->
//! ## Refresh endpoint
//! ```
//!
//! A section entity's body runs to the next heading of equal-or-shallower depth or the next
//! entity marker; the file-level entity's body stops at the first marker. Parsing never fails:
//! problems become diagnostics and the rest of the file is still read.

use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::wiki::diagnostics::{diag, DiagExt};
use crate::wiki::markdown::{
    line_at, line_starts, scan, Anchor, EntityMarker, Heading, ScannedDoc,
};
use crate::wiki::models::{
    is_source_type, normalize_lifecycle, CommittedGrounding, EntityRelation, EntityTypeRegistry,
    Provenance, GROUNDING_ORIGIN_ANCHOR, GROUNDING_ORIGIN_FRONTMATTER,
    WikiDiagnostic, WikiEntity, WikiSource, PARENT_TOPIC_RELATION, RELATION_TYPES,
};

/// Where an entity sits in its file (byte offsets). The three ranges are contiguous with any
/// blank gap between metadata and heading.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityLocation {
    pub kind: MetadataKind,
    pub metadata_start: usize,
    pub metadata_end: usize,
    pub heading_start: usize,
    pub heading_end: usize,
    pub body_start: usize,
    pub body_end: usize,
    /// YAML region holding the entity's keys (frontmatter inner text or marker YAML lines).
    pub yaml_start: usize,
    pub yaml_end: usize,
    /// Marker written with inline `key=value` attributes (rewritten to block form on edit).
    pub inline_attrs: bool,
    pub heading_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataKind {
    Frontmatter,
    Marker,
    Implicit,
}

impl MetadataKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MetadataKind::Frontmatter => "frontmatter",
            MetadataKind::Marker => "marker",
            MetadataKind::Implicit => "implicit",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedEntity {
    pub entity: WikiEntity,
    pub loc: EntityLocation,
    /// The raw metadata map as read (keys in file order are not guaranteed).
    pub raw: Map<String, Value>,
    /// Anchor references found in this entity's body.
    pub anchor_refs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: String,
    pub text: String,
    pub doc: ScannedDoc,
    pub entities: Vec<ParsedEntity>,
    pub diagnostics: Vec<WikiDiagnostic>,
    pub file_hash: String,
}

impl ParsedFile {
    pub fn file_entity(&self) -> Option<&ParsedEntity> {
        self.entities
            .iter()
            .find(|e| e.loc.kind != MetadataKind::Marker)
    }

    pub fn entity(&self, id: &str) -> Option<&ParsedEntity> {
        self.entities.iter().find(|e| e.entity.id == id)
    }

    /// Exact text of one entity (metadata + heading + body).
    pub fn entity_text(&self, loc: &EntityLocation) -> String {
        entity_text_of(&self.text, loc)
    }
}

pub fn entity_text_of(text: &str, loc: &EntityLocation) -> String {
    let mut s = String::new();
    s.push_str(&text[loc.metadata_start..loc.metadata_end]);
    s.push_str(&text[loc.heading_start..loc.heading_end]);
    s.push_str(&text[loc.body_start..loc.body_end]);
    s
}

/// SHA-256 of `text` with CRLF normalized to LF and trailing whitespace ignored (blank lines
/// added after an entity when a neighbour is inserted do not change it).
pub fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.replace("\r\n", "\n").trim_end().as_bytes());
    hex::encode(h.finalize())
}

pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('_');
            dash = true;
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_end_matches('_').to_string();
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
    }
}

/// Entity ids are one token: letters, digits, `_`, `-`, `.`, `:`.
pub fn is_valid_entity_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

pub fn infer_type_from_path(file: &str) -> String {
    if file.contains("specs") {
        "spec"
    } else if file.contains("patterns") {
        "pattern"
    } else if file.contains("topics") {
        "topic"
    } else if file.contains("context") {
        "architecture"
    } else if file.contains("decisions") {
        "decision"
    } else {
        "document"
    }
    .to_string()
}

fn file_stem(file: &str) -> String {
    Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("entity")
        .to_string()
}

/// Parse inline marker attributes: `key=value key="quoted value" topics=[a, b]`.
pub fn parse_inline_attrs(attrs: &str) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    let chars: Vec<char> = attrs.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let ks = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        let key: String = chars[ks..i].iter().collect();
        if i >= chars.len() || chars[i] != '=' {
            return Err(format!("attribute `{}` has no `=value`", key));
        }
        i += 1;
        if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
            let q = chars[i];
            i += 1;
            let vs = i;
            while i < chars.len() && chars[i] != q {
                i += 1;
            }
            if i >= chars.len() {
                return Err(format!("attribute `{}` has an unterminated quote", key));
            }
            let inner: String = chars[vs..i].iter().collect();
            i += 1;
            out.insert(key, Value::String(inner));
            continue;
        } else {
            let vs = i;
            let mut depth = 0i32;
            while i < chars.len() {
                match chars[i] {
                    '[' | '{' => depth += 1,
                    ']' | '}' => depth -= 1,
                    c if c.is_whitespace() && depth <= 0 => break,
                    _ => {}
                }
                i += 1;
            }
            let raw: String = chars[vs..i].iter().collect();
            let value = serde_yaml::from_str::<serde_yaml::Value>(&raw)
                .ok()
                .and_then(|v| serde_json::to_value(v).ok())
                .unwrap_or(Value::String(raw.clone()));
            out.insert(key, value);
        }
    }
    Ok(out)
}

fn yaml_to_map(yaml: &str) -> Result<Map<String, Value>, (String, Option<usize>)> {
    if yaml.trim().is_empty() {
        return Ok(Map::new());
    }
    let v: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| (e.to_string(), crate::drift::markdown::yaml_error_line(yaml, &e)))?;
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(Value::Null) => Ok(Map::new()),
        Ok(_) => Err(("metadata is not a key/value map".to_string(), None)),
        Err(e) => Err((e.to_string(), None)),
    }
}

/// Strings from a value that is a string, a comma-separated string, or a list of strings.
fn string_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Null => Some(Vec::new()),
        Value::String(s) => Some(
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
        ),
        Value::Array(a) => {
            let mut out = Vec::new();
            for e in a {
                match e {
                    Value::String(s) if !s.trim().is_empty() => out.push(s.trim().to_string()),
                    Value::Number(n) => out.push(n.to_string()),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

struct Reader<'a> {
    file: &'a str,
    line: usize,
    entity_id: String,
    diags: Vec<WikiDiagnostic>,
}

impl Reader<'_> {
    fn push(&mut self, code: &str, msg: String, path: &str) {
        self.diags.push(
            diag(code, msg, self.file)
                .at_line(Some(self.line))
                .for_entity(&self.entity_id)
                .at_path(path),
        );
    }
}

fn read_relations(m: &Map<String, Value>, r: &mut Reader) -> Vec<EntityRelation> {
    let mut out = Vec::new();
    if let Some(v) = m.get("relations") {
        match v {
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let path = format!("relations[{}]", i);
                    let Some(obj) = item.as_object() else {
                        r.push(
                            "INVALID_FIELD_TYPE",
                            "A relation is a map with `type` and `target_id`".to_string(),
                            &path,
                        );
                        continue;
                    };
                    let rel_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let target = obj
                        .get("target_id")
                        .or_else(|| obj.get("target"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if rel_type.is_empty() || target.is_empty() {
                        r.push(
                            "INVALID_FIELD_TYPE",
                            "A relation needs both `type` and `target_id`".to_string(),
                            &path,
                        );
                        continue;
                    }
                    let metadata = obj.get("metadata").filter(|v| v.is_object()).cloned();
                    let waived = obj.get("waived").and_then(|v| v.as_bool()).unwrap_or(false)
                        || metadata
                            .as_ref()
                            .and_then(|m| m.get("waived"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                    out.push(EntityRelation {
                        rel_type: rel_type.trim().to_string(),
                        target_id: target.trim().to_string(),
                        note: obj
                            .get("note")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        waived,
                        metadata,
                        origin: None,
                    });
                }
            }
            Value::Null => {}
            _ => r.push(
                "INVALID_FIELD_TYPE",
                "`relations` must be a list".to_string(),
                "relations",
            ),
        }
    }
    // Shorthand keys: `implements: [kb_x]`, `parent: kb_topic`, ...
    let mut keys: Vec<&str> = RELATION_TYPES.to_vec();
    keys.push("parent");
    for key in keys {
        let Some(v) = m.get(key) else { continue };
        let rel_type = if key == "parent" {
            PARENT_TOPIC_RELATION
        } else {
            key
        };
        match string_list(v) {
            Some(targets) => {
                for t in targets {
                    out.push(EntityRelation {
                        rel_type: rel_type.to_string(),
                        target_id: t,
                        origin: Some(key.to_string()),
                        ..Default::default()
                    });
                }
            }
            None => r.push(
                "INVALID_FIELD_TYPE",
                format!("`{}` must be an entity id or a list of entity ids", key),
                key,
            ),
        }
    }
    out
}

fn committed_value(o: &Map<String, Value>, key: &str) -> Option<String> {
    match o.get(key)? {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

/// `grounds_to` references, plus the baselines committed with map entries.
fn read_groundings(
    m: &Map<String, Value>,
    r: &mut Reader,
) -> (Vec<String>, Vec<CommittedGrounding>) {
    let mut out: Vec<String> = Vec::new();
    let mut committed: Vec<CommittedGrounding> = Vec::new();
    let Some(v) = m.get("grounds_to") else {
        return (out, committed);
    };
    let items: Vec<Value> = match v {
        Value::Array(a) => a.clone(),
        Value::String(_) => vec![v.clone()],
        Value::Null => Vec::new(),
        _ => {
            r.push(
                "MALFORMED_GROUNDING",
                "`grounds_to` must be a list of code references".to_string(),
                "grounds_to",
            );
            return (out, committed);
        }
    };
    for (i, g) in items.iter().enumerate() {
        let reference = match g {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Object(o) => o
                .get("node_id")
                .or_else(|| o.get("node"))
                .or_else(|| o.get("ref"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string()),
            _ => None,
        };
        match reference.filter(|s| !s.is_empty()) {
            Some(id) => {
                if !out.contains(&id) {
                    let (body_hash, fingerprint) = match g {
                        Value::Object(o) => (
                            committed_value(o, "body_hash"),
                            committed_value(o, "fingerprint"),
                        ),
                        _ => (None, None),
                    };
                    committed.push(CommittedGrounding {
                        reference: id.clone(),
                        origin: GROUNDING_ORIGIN_FRONTMATTER.to_string(),
                        body_hash,
                        fingerprint,
                    });
                    out.push(id);
                }
            }
            None => r.push(
                "MALFORMED_GROUNDING",
                "A grounding is a readable reference `kind:path:qualified_name` or a map with `node_id`".to_string(),
                &format!("grounds_to[{}]", i),
            ),
        }
    }
    (out, committed)
}

fn read_sources(m: &Map<String, Value>, r: &mut Reader) -> Vec<WikiSource> {
    let mut out = Vec::new();
    let Some(v) = m.get("sources") else {
        return out;
    };
    let Some(items) = v.as_array() else {
        if !v.is_null() {
            r.push(
                "MALFORMED_SOURCE",
                "`sources` must be a list".to_string(),
                "sources",
            );
        }
        return out;
    };
    for (i, item) in items.iter().enumerate() {
        let path = format!("sources[{}]", i);
        let parsed = match item {
            Value::String(s) => s.split_once(':').and_then(|(t, rest)| {
                is_source_type(t.trim()).then(|| WikiSource {
                    source_type: t.trim().to_string(),
                    reference: Some(rest.trim().to_string()),
                    ..Default::default()
                })
            }),
            Value::Object(_) => serde_json::from_value::<WikiSource>(item.clone()).ok(),
            _ => None,
        };
        match parsed {
            Some(s) => out.push(s),
            None => r.push(
                "MALFORMED_SOURCE",
                "A source is a map with `type` (and `ref`/`note`), or a `type:ref` string"
                    .to_string(),
                &path,
            ),
        }
    }
    out
}

/// Build one entity from its metadata map.
#[allow(clippy::too_many_arguments)]
fn build_entity(
    file: &str,
    text: &str,
    starts: &[usize],
    meta: Map<String, Value>,
    loc: EntityLocation,
    heading: Option<&Heading>,
    registry: &EntityTypeRegistry,
    first_h1: Option<String>,
) -> (ParsedEntity, Vec<WikiDiagnostic>) {
    let line = line_at(starts, loc.metadata_start);
    let is_marker = loc.kind == MetadataKind::Marker;
    let stem = file_stem(file);
    let heading_title = heading.map(|h| h.title.clone());
    let title = meta
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| heading_title.clone().filter(|s| !s.is_empty()))
        .or(if is_marker { None } else { first_h1 })
        .unwrap_or_else(|| stem.clone());

    let mut r = Reader {
        file,
        line,
        entity_id: String::new(),
        diags: Vec::new(),
    };

    let id = match meta.get("id") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => {
            if is_marker {
                let derived = format!("kb_{}_{}", slugify(&stem), slugify(&title));
                r.entity_id = derived.clone();
                r.push(
                    "MISSING_REQUIRED_FIELD",
                    format!(
                        "Entity marker for \"{}\" has no `id`; using derived id {}",
                        title, derived
                    ),
                    "id",
                );
                derived
            } else {
                format!("kb_{}", stem)
            }
        }
    };
    r.entity_id = id.clone();
    if !is_valid_entity_id(&id) {
        r.push(
            "INVALID_ENTITY_ID",
            format!("Entity id {:?} is not a single id token", id),
            "id",
        );
    }

    let entity_type = match meta.get("type").and_then(|v| v.as_str()).map(|s| s.trim()) {
        Some(t) if !t.is_empty() => {
            if !registry.has(t) {
                r.push(
                    "INVALID_ENTITY_TYPE",
                    format!(
                        "Unknown entity type {:?}. Registered: {}",
                        t,
                        registry.list().join(", ")
                    ),
                    "type",
                );
            }
            t.to_string()
        }
        _ => infer_type_from_path(file),
    };

    let status = match meta.get("status") {
        None | Some(Value::Null) => "promoted".to_string(),
        Some(v) => {
            let raw = v
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string());
            match normalize_lifecycle(&raw) {
                Some((s, false)) => s.to_string(),
                Some((s, true)) => {
                    r.push(
                        "LEGACY_LIFECYCLE_STATE",
                        format!("Status {:?} was read as lifecycle state {:?}", raw, s),
                        "status",
                    );
                    s.to_string()
                }
                None => {
                    r.push(
                        "INVALID_LIFECYCLE_STATE",
                        format!(
                            "Unknown lifecycle state {:?}; treated as promoted. Grounding health (fresh/changed/missing) is derived, never stored here",
                            raw
                        ),
                        "status",
                    );
                    "promoted".to_string()
                }
            }
        }
    };

    let revision = match meta.get("revision") {
        None | Some(Value::Null) => 1,
        Some(v) => match v.as_i64() {
            Some(n) if n >= 1 => n,
            _ => {
                r.push(
                    "INVALID_REVISION",
                    format!("Revision must be an integer >= 1, got {}", v),
                    "revision",
                );
                1
            }
        },
    };

    let summary = meta
        .get("summary")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let topics = match meta.get("topics") {
        None => Vec::new(),
        Some(v) => string_list(v).unwrap_or_else(|| {
            r.push(
                "INVALID_FIELD_TYPE",
                "`topics` must be a list of topic ids or names".to_string(),
                "topics",
            );
            Vec::new()
        }),
    };
    let aliases = match meta.get("aliases") {
        None => Vec::new(),
        Some(v) => string_list(v).unwrap_or_else(|| {
            r.push(
                "INVALID_FIELD_TYPE",
                "`aliases` must be a list of names".to_string(),
                "aliases",
            );
            Vec::new()
        }),
    };
    let relations = read_relations(&meta, &mut r);
    let (grounds_to, committed_groundings) = read_groundings(&meta, &mut r);
    let sources = read_sources(&meta, &mut r);
    let provenance = match meta.get("provenance") {
        None | Some(Value::Null) => None,
        Some(v) => match serde_json::from_value::<Provenance>(v.clone()) {
            Ok(p) => Some(p),
            Err(e) => {
                r.push(
                    "INVALID_FIELD_TYPE",
                    format!("`provenance` is malformed ({}); it is ignored", e),
                    "provenance",
                );
                None
            }
        },
    };
    let metadata = match meta.get("metadata") {
        None | Some(Value::Null) => None,
        Some(v) if v.is_object() => Some(v.clone()),
        Some(_) => {
            r.push(
                "INVALID_FIELD_TYPE",
                "`metadata` must be a map".to_string(),
                "metadata",
            );
            None
        }
    };

    let body = text[loc.body_start..loc.body_end].to_string();
    let hash = content_hash(&entity_text_of(text, &loc));
    let start_line = line_at(starts, loc.metadata_start.min(loc.heading_start));
    let end_line = line_at(
        starts,
        loc.body_end.saturating_sub(1).max(loc.metadata_start),
    );
    let entity = WikiEntity {
        entity_key: format!(
            "{}#{}",
            file,
            if is_marker { loc.metadata_start } else { 0 }
        ),
        id,
        file: file.to_string(),
        entity_type,
        title,
        summary,
        body,
        status,
        revision,
        relations,
        grounds_to,
        committed_groundings,
        topics,
        sources,
        provenance,
        aliases,
        metadata,
        health: None,
        start_line,
        end_line: end_line.max(start_line),
        heading_depth: loc.heading_depth,
        content_hash: hash,
        metadata_kind: loc.kind.as_str().to_string(),
    };
    (
        ParsedEntity {
            entity,
            loc,
            raw: meta,
            anchor_refs: Vec::new(),
        },
        r.diags,
    )
}

fn marker_meta(
    text: &str,
    m: &EntityMarker,
) -> Result<Map<String, Value>, (String, Option<usize>)> {
    let mut meta = yaml_to_map(&text[m.yaml_start..m.yaml_end])?;
    if !m.attrs.is_empty() {
        let attrs = parse_inline_attrs(&m.attrs).map_err(|e| (e, None))?;
        for (k, v) in attrs {
            meta.insert(k, v);
        }
    }
    Ok(meta)
}

fn heading_after<'a>(
    text: &str,
    doc: &'a ScannedDoc,
    from: usize,
    skip_markers: bool,
) -> Option<&'a Heading> {
    let h = doc.headings.iter().find(|h| h.start >= from)?;
    // Only blank lines (and, when allowed, other markers) may separate the two.
    let mut cursor = from;
    let mut residual = String::new();
    if skip_markers {
        for m in &doc.markers {
            if m.start >= cursor && m.end <= h.start {
                residual.push_str(&text[cursor..m.start]);
                cursor = m.end;
            }
        }
    }
    residual.push_str(&text[cursor..h.start]);
    residual.trim().is_empty().then_some(h)
}

/// Parse one file with the default type registry.
pub fn parse_markdown_file(file: &str, text: &str) -> ParsedFile {
    parse_markdown_file_with(file, text, &EntityTypeRegistry::default())
}

pub fn parse_markdown_file_with(
    file: &str,
    text: &str,
    registry: &EntityTypeRegistry,
) -> ParsedFile {
    let doc = scan(text);
    let starts = line_starts(text);
    let mut diagnostics: Vec<WikiDiagnostic> = Vec::new();
    for p in &doc.problems {
        diagnostics
            .push(diag(p.code, p.message.clone(), file).at_line(Some(line_at(&starts, p.offset))));
    }

    // Marker bindings.
    let mut claims: Vec<(usize, Vec<usize>)> = Vec::new(); // heading index -> marker indices
    let mut unbound: Vec<usize> = Vec::new();
    for (mi, m) in doc.markers.iter().enumerate() {
        match heading_after(text, &doc, m.end, true) {
            Some(h) => {
                let hi = doc
                    .headings
                    .iter()
                    .position(|x| x.start == h.start)
                    .unwrap_or(0);
                match claims.iter_mut().find(|(k, _)| *k == hi) {
                    Some((_, v)) => v.push(mi),
                    None => claims.push((hi, vec![mi])),
                }
            }
            None => unbound.push(mi),
        }
    }
    for mi in &unbound {
        diagnostics.push(
            diag(
                "UNBOUND_ENTITY_METADATA",
                format!("Entity marker in {} is not followed by a heading", file),
                file,
            )
            .at_line(Some(line_at(&starts, doc.markers[*mi].start))),
        );
    }
    let marker_starts: Vec<usize> = doc.markers.iter().map(|m| m.start).collect();
    let first_marker_after =
        |from: usize| marker_starts.iter().copied().filter(|s| *s >= from).min();

    let mut built: Vec<ParsedEntity> = Vec::new();
    let first_h1 = doc
        .headings
        .iter()
        .find(|h| h.depth == 1)
        .map(|h| h.title.clone());

    // File-level entity.
    {
        let (kind, ms, me, ys, ye, meta) = match &doc.frontmatter {
            Some(fm) => {
                let meta = match yaml_to_map(&text[fm.inner_start..fm.inner_end]) {
                    Ok(m) => m,
                    Err((msg, line)) => {
                        let fm_line = line_at(&starts, fm.start);
                        diagnostics.push(
                            diag(
                                "FRONTMATTER_PARSE_ERROR",
                                format!("Invalid YAML frontmatter (fields ignored): {}", msg),
                                file,
                            )
                            .at_line(Some(line.map(|l| fm_line + l).unwrap_or(fm_line))),
                        );
                        Map::new()
                    }
                };
                (
                    MetadataKind::Frontmatter,
                    fm.start,
                    fm.end,
                    fm.inner_start,
                    fm.inner_end,
                    meta,
                )
            }
            None => (
                MetadataKind::Implicit,
                doc.bom,
                doc.bom,
                doc.bom,
                doc.bom,
                Map::new(),
            ),
        };
        let heading = heading_after(text, &doc, me, false).filter(|h| {
            !claims
                .iter()
                .any(|(hi, _)| doc.headings[*hi].start == h.start)
        });
        let body_start = heading.map(|h| h.end).unwrap_or(me);
        let body_end = first_marker_after(body_start)
            .unwrap_or(text.len())
            .max(body_start);
        let loc = EntityLocation {
            kind,
            metadata_start: ms,
            metadata_end: me,
            heading_start: heading.map(|h| h.start).unwrap_or(body_start),
            heading_end: heading.map(|h| h.end).unwrap_or(body_start),
            body_start,
            body_end,
            yaml_start: ys,
            yaml_end: ye,
            inline_attrs: false,
            heading_depth: heading.map(|h| h.depth).unwrap_or(0),
        };
        let (pe, d) = build_entity(
            file,
            text,
            &starts,
            meta,
            loc,
            heading,
            registry,
            first_h1.clone(),
        );
        diagnostics.extend(d);
        built.push(pe);
    }

    // Section entities.
    claims.sort_by_key(|(hi, _)| *hi);
    for (hi, mis) in &claims {
        let h = &doc.headings[*hi];
        if mis.len() > 1 {
            diagnostics.push(
                diag(
                    "DUPLICATE_ENTITY_METADATA",
                    format!(
                        "{} entity markers in {} bind to the heading {:?}",
                        mis.len(),
                        file,
                        h.title
                    ),
                    file,
                )
                .at_line(Some(line_at(&starts, doc.markers[mis[0]].start))),
            );
            continue;
        }
        let m = &doc.markers[mis[0]];
        let meta = match marker_meta(text, m) {
            Ok(meta) => meta,
            Err((msg, _)) => {
                diagnostics.push(
                    diag(
                        "WIKI_PARSE_ERROR",
                        format!("Malformed entity marker metadata in {}: {}", file, msg),
                        file,
                    )
                    .at_line(Some(line_at(&starts, m.start))),
                );
                continue;
            }
        };
        let body_start = h.end;
        let mut body_end = text.len();
        if let Some(next) = doc
            .headings
            .iter()
            .find(|x| x.start >= body_start && x.depth <= h.depth)
        {
            body_end = next.start;
        }
        if let Some(ns) = marker_starts
            .iter()
            .copied()
            .filter(|s| *s >= body_start && *s > m.start)
            .min()
        {
            body_end = body_end.min(ns);
        }
        let loc = EntityLocation {
            kind: MetadataKind::Marker,
            metadata_start: m.start,
            metadata_end: m.end,
            heading_start: h.start,
            heading_end: h.end,
            body_start,
            body_end: body_end.max(body_start),
            yaml_start: m.yaml_start,
            yaml_end: m.yaml_end,
            inline_attrs: !m.attrs.is_empty(),
            heading_depth: h.depth,
        };
        let (pe, d) = build_entity(file, text, &starts, meta, loc, Some(h), registry, None);
        diagnostics.extend(d);
        built.push(pe);
    }

    // Anchors attach to the entity whose body contains them, else to the file-level entity.
    for a in &doc.anchors {
        if !attach_anchor(&mut built, a) {
            diagnostics.push(
                diag(
                    "UNBOUND_ANCHOR",
                    format!(
                        "The kb-ground anchor `{}` in {} is outside every entity's section and is not attached",
                        a.reference, file
                    ),
                    file,
                )
                .at_line(Some(line_at(&starts, a.start))),
            );
        }
    }

    // Precise positions: the metadata key a diagnostic names, else its line.
    {
        let map = crate::wiki::positions::PositionMap::new(text);
        for d in diagnostics.iter_mut() {
            let loc = d
                .entity_id
                .as_deref()
                .and_then(|id| built.iter().find(|e| e.entity.id == id))
                .map(|e| &e.loc);
            crate::wiki::positions::locate_diagnostic(d, &map, loc);
        }
    }

    ParsedFile {
        path: file.to_string(),
        text: text.to_string(),
        file_hash: crate::graph::fingerprint::compute_file_hash(text.as_bytes()),
        doc,
        entities: built,
        diagnostics,
    }
}

/// Attach an anchor to the marker entity whose body contains it, else to the file-level
/// entity when the anchor lies in its region (before the first marker). An anchor outside
/// every entity's range (e.g. under a plain heading after a marker section) is not attached
/// to an arbitrary entity: returns false so the caller reports it as unbound.
fn attach_anchor(entities: &mut [ParsedEntity], a: &Anchor) -> bool {
    let idx = entities
        .iter()
        .position(|e| {
            e.loc.kind == MetadataKind::Marker
                && a.start >= e.loc.body_start
                && a.start < e.loc.body_end
        })
        .or_else(|| {
            entities
                .iter()
                .position(|e| e.loc.kind != MetadataKind::Marker && a.start < e.loc.body_end)
        });
    let Some(idx) = idx else {
        return false;
    };
    if let Some(e) = entities.get_mut(idx) {
        e.anchor_refs.push(a.reference.clone());
        if !e.entity.grounds_to.contains(&a.reference) {
            e.entity.grounds_to.push(a.reference.clone());
            e.entity.committed_groundings.push(CommittedGrounding {
                reference: a.reference.clone(),
                origin: GROUNDING_ORIGIN_ANCHOR.to_string(),
                body_hash: a.body_hash.clone(),
                fingerprint: None,
            });
        }
    }
    true
}

/// Document-level view (back-compat): the file-level entity, with the groundings of every
/// entity in the file.
pub fn parse_markdown_entity(file_rel_path: &str, content: &str) -> Option<WikiEntity> {
    parse_markdown_entity_with_diagnostics(file_rel_path, content).0
}

/// Like [`parse_markdown_entity`], plus the file's parse diagnostics.
pub fn parse_markdown_entity_with_diagnostics(
    file_rel_path: &str,
    content: &str,
) -> (Option<WikiEntity>, Vec<WikiDiagnostic>) {
    let parsed = parse_markdown_file(file_rel_path, content);
    let mut all_refs: Vec<String> = Vec::new();
    let mut all_committed: Vec<CommittedGrounding> = Vec::new();
    for e in &parsed.entities {
        for g in &e.entity.grounds_to {
            if !all_refs.contains(g) {
                all_refs.push(g.clone());
                if let Some(c) = e.entity.committed_for(g) {
                    all_committed.push(c.clone());
                }
            }
        }
    }
    let mut file_entity = parsed.file_entity().map(|e| e.entity.clone());
    if let Some(fe) = file_entity.as_mut() {
        fe.grounds_to = all_refs;
        fe.committed_groundings = all_committed;
    }
    (file_entity, parsed.diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_grounding_baselines_are_carried() {
        let text = "---\nid: kb_a\ntitle: A\ngrounds_to:\n  - ref: function:src/a.rs:a\n    body_hash: h1\n    fingerprint: mh1:3:00\n  - function:src/a.rs:b\n---\n# A\n\nBody.\n<!-- kb-ground: function:src/a.rs:c #h3 -->\n";
        let p = parse_markdown_file("a.md", text);
        let e = &p.entities[0].entity;
        assert_eq!(e.grounds_to.len(), 3);
        let a = e.committed_for("function:src/a.rs:a").unwrap();
        assert_eq!(a.body_hash.as_deref(), Some("h1"));
        assert_eq!(a.fingerprint.as_deref(), Some("mh1:3:00"));
        assert_eq!(a.origin, GROUNDING_ORIGIN_FRONTMATTER);
        assert_eq!(e.committed_for("function:src/a.rs:b").unwrap().body_hash, None);
        let c = e.committed_for("function:src/a.rs:c").unwrap();
        assert_eq!(c.body_hash.as_deref(), Some("h3"));
        assert_eq!(c.origin, GROUNDING_ORIGIN_ANCHOR);
        let fe = parse_markdown_entity("a.md", text).unwrap();
        assert_eq!(fe.committed_groundings.len(), 3);
    }

    #[test]
    fn multiple_entities_per_file() {
        let text = "---\nid: kb_auth\ntitle: Auth\n---\n# Auth\n\nIntro.\n\n<!-- kb:entity id=kb_ttl type=decision status=promoted -->\n## Token TTL\n\nFifteen minutes.\n\n### Detail\n\nmore\n\n<!-- kb:entity\nid: kb_refresh\ntype: component\nimplements: [kb_ttl]\n-->\n## Refresh\n\nRefresh body.\n<!-- kb-ground: function:src/a.rs:refresh -->\n";
        let p = parse_markdown_file("context/auth.md", text);
        let ids: Vec<&str> = p.entities.iter().map(|e| e.entity.id.as_str()).collect();
        assert_eq!(ids, vec!["kb_auth", "kb_ttl", "kb_refresh"]);
        assert_eq!(p.entities[0].entity.body.trim(), "Intro.");
        assert!(p.entities[1].entity.body.contains("### Detail"));
        assert_eq!(p.entities[2].entity.relations[0].rel_type, "implements");
        assert_eq!(
            p.entities[2].entity.grounds_to,
            vec!["function:src/a.rs:refresh"]
        );
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    }

    #[test]
    fn unbound_marker_is_reported() {
        let text = "# A\n\n<!-- kb:entity id=kb_x -->\ntext without heading\n";
        let p = parse_markdown_file("a.md", text);
        assert!(p
            .diagnostics
            .iter()
            .any(|d| d.code == "UNBOUND_ENTITY_METADATA"));
    }
}
