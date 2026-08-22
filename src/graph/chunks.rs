//! Source-chunk full-text index: every indexed file is split into overlapping line windows
//! (80 lines, stride 60) whose identifier components, comments and path are indexed in the
//! FTS5 table `code_chunks`. Scope uses it as an independent retrieval channel that reaches
//! bodies and comments, not only declaration names.

use rusqlite::{params, Connection, Result};
use std::collections::HashSet;

pub const CHUNK_WINDOW: usize = 80;
pub const CHUNK_STRIDE: usize = 60;
/// Declarations reported per chunk hit.
const CHUNK_NODE_LIMIT: usize = 8;

/// One indexed window.
#[derive(Debug, Clone)]
pub(crate) struct ChunkRow {
    pub start_line: i64,
    pub end_line: i64,
    pub path_terms: String,
    pub identifier_terms: String,
    pub comment_terms: String,
}

/// A source window matching a query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChunkHit {
    pub file_path: String,
    pub start_line: i64,
    pub end_line: i64,
    /// Negative fused relevance (more negative is better), like FTS5 `rank`.
    pub rank: f64,
    pub matched_terms: Vec<String>,
    /// Named declarations overlapping the window, most specific first.
    pub node_ids: Vec<String>,
}

/// Lowercase identifier tokens with their camelCase / snake_case components.
pub fn split_index_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut token = String::new();
    let mut chars = text.chars().peekable();
    loop {
        let next = chars.next();
        match next {
            Some(ch) if ch.is_alphanumeric() || ch == '_' => token.push(ch),
            _ => {
                if !token.is_empty() {
                    let t = std::mem::take(&mut token);
                    let mut all = vec![t.to_lowercase()];
                    all.extend(identifier_components(&t));
                    for w in all {
                        if w.len() >= 2 && seen.insert(w.clone()) {
                            out.push(w);
                        }
                    }
                }
                if next.is_none() {
                    break;
                }
            }
        }
    }
    out
}

/// `BudgetLedger` -> [`budget`, `ledger`]; `parse_http_url` -> [`parse`, `http`, `url`].
pub fn identifier_components(raw: &str) -> Vec<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if c == '_' || c == '$' || c == '-' {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if !cur.is_empty() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (c.is_uppercase() && (prev.is_lowercase() || prev.is_ascii_digit()))
                || (c.is_uppercase() && prev.is_uppercase() && next_lower);
            if boundary {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let l = p.to_lowercase();
        if l.len() >= 2 && !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with('#') || t.starts_with("/*") || t.starts_with('*')
        || t.starts_with("--") || t.starts_with("\"\"\"") || t.starts_with("'''")
}

/// Overlapping windows of a file.
pub(crate) fn chunk_source(path: &str, content: &str) -> Vec<ChunkRow> {
    let lines: Vec<&str> = content.lines().collect();
    let path_terms = split_index_terms(path).join(" ");
    let mut out = Vec::new();
    let mut offset = 0;
    loop {
        let end = (offset + CHUNK_WINDOW).min(lines.len());
        if offset >= end && !out.is_empty() {
            break;
        }
        let window = &lines[offset.min(lines.len())..end];
        let text = window.join("\n");
        let comments: Vec<&str> = window.iter().copied().filter(|l| is_comment_line(l)).collect();
        out.push(ChunkRow {
            start_line: offset as i64 + 1,
            end_line: end.max(offset + 1) as i64,
            path_terms: path_terms.clone(),
            identifier_terms: split_index_terms(&text).join(" "),
            comment_terms: split_index_terms(&comments.join(" ")).join(" "),
        });
        if end >= lines.len() {
            break;
        }
        offset += CHUNK_STRIDE;
    }
    out
}

pub(crate) fn insert_chunks(conn: &Connection, path: &str, content_hash: &str, rows: &[ChunkRow]) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO code_chunks (file_path, content_hash, start_line, end_line, path_terms, identifier_terms, comment_terms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for r in rows {
        stmt.execute(params![
            path,
            content_hash,
            r.start_line,
            r.end_line,
            r.path_terms,
            r.identifier_terms,
            r.comment_terms
        ])?;
    }
    Ok(())
}

/// Chunks of `path` at `content_hash` already indexed in `conn` (a live graph), if any.
pub(crate) fn existing_chunks(conn: &Connection, path: &str, content_hash: &str) -> Option<Vec<ChunkRow>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT start_line, end_line, path_terms, identifier_terms, comment_terms FROM code_chunks \
             WHERE file_path = ?1 AND content_hash = ?2 ORDER BY start_line",
        )
        .ok()?;
    let rows: Vec<ChunkRow> = stmt
        .query_map(params![path, content_hash], |r| {
            Ok(ChunkRow {
                start_line: r.get(0)?,
                end_line: r.get(1)?,
                path_terms: r.get(2)?,
                identifier_terms: r.get(3)?,
                comment_terms: r.get(4)?,
            })
        })
        .ok()?
        .flatten()
        .collect();
    (!rows.is_empty()).then_some(rows)
}

/// Search chunks for weighted terms (term, weight, prefix); one FTS query per term fused by
/// weighted reciprocal rank. Deterministic: ties break on path and line.
pub fn search_chunks(conn: &Connection, terms: &[(String, f64, bool)], limit: usize) -> Vec<ChunkHit> {
    use std::collections::HashMap;
    let mut fused: HashMap<(String, i64), (ChunkHit, f64)> = HashMap::new();
    let Ok(mut stmt) = conn.prepare_cached(
        "SELECT file_path, start_line, end_line FROM code_chunks WHERE code_chunks MATCH ?1 \
         ORDER BY bm25(code_chunks, 0, 0, 0, 0, 4, 2, 3), file_path, start_line LIMIT ?2",
    ) else {
        return Vec::new();
    };
    for (term, weight, prefix) in terms.iter().take(32) {
        let escaped = term.replace('"', "");
        if escaped.is_empty() {
            continue;
        }
        let m = if *prefix && escaped.len() > 3 {
            format!("{{path_terms identifier_terms comment_terms}} : \"{}\"*", escaped)
        } else {
            format!("{{path_terms identifier_terms comment_terms}} : \"{}\"", escaped)
        };
        let rows: Vec<(String, i64, i64)> = match stmt.query_map(params![m, limit.max(20) as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        }) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => continue,
        };
        for (i, (file, start, end)) in rows.into_iter().enumerate() {
            let e = fused.entry((file.clone(), start)).or_insert_with(|| {
                (
                    ChunkHit {
                        file_path: file,
                        start_line: start,
                        end_line: end,
                        rank: 0.0,
                        matched_terms: Vec::new(),
                        node_ids: Vec::new(),
                    },
                    0.0,
                )
            });
            e.1 += weight / (60.0 + i as f64 + 1.0);
            if !e.0.matched_terms.contains(term) {
                e.0.matched_terms.push(term.clone());
            }
        }
    }
    let mut hits: Vec<(ChunkHit, f64)> = fused.into_values().collect();
    hits.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.file_path.cmp(&b.0.file_path))
            .then(a.0.start_line.cmp(&b.0.start_line))
    });
    hits.truncate(limit);
    let mut node_stmt = conn
        .prepare_cached(
            "SELECT id FROM nodes WHERE file_path = ?1 AND start_line <= ?2 AND end_line >= ?3 \
             AND kind NOT IN ('file', 'module', 'parameter', 'route', 'enum_member') AND trim(name) <> '' \
             ORDER BY CASE WHEN kind IN ('function', 'method', 'class', 'struct', 'interface', 'trait', 'enum') THEN 0 ELSE 1 END, \
                      (min(end_line, ?2) - max(start_line, ?3) + 1) DESC, (end_line - start_line) ASC, start_line, id \
             LIMIT ?4",
        )
        .ok();
    hits.into_iter()
        .map(|(mut h, score)| {
            h.rank = -score;
            h.matched_terms.sort();
            if let Some(s) = node_stmt.as_mut() {
                if let Ok(rows) = s.query_map(
                    params![h.file_path, h.end_line, h.start_line, CHUNK_NODE_LIMIT as i64],
                    |r| r.get::<_, String>(0),
                ) {
                    h.node_ids = rows.flatten().collect();
                }
            }
            h
        })
        .collect()
}
