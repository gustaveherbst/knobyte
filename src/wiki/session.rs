//! Contract read session: the read surface agents (MCP) and other adapters use.
//!
//! * **Snapshot-bound.** A session opens the index strictly read-only and holds one read
//!   transaction for its lifetime, so every read in it sees the same index generation, even if
//!   a refresh publishes a new one meanwhile.
//! * **Revision-bound cursors.** Paged answers carry an opaque `nextCursor` that names the
//!   operation, a hash of the request, the indexed revision and the corpus revision. A cursor
//!   from another request is `INVALID_REQUEST`; one issued before the index or the Markdown
//!   changed is refused with `REVISION_CONFLICT` (restart without a cursor).
//! * **State reporting.** Every session reports the index's state (one of the seven
//!   [`crate::wiki::maintenance::INDEX_STATES`]); reads are refused when the index is
//!   `missing`, `corrupt` or `rebuild_required`.
//! * **Traversal options.** Relations and neighbourhoods take a `direction` (`outgoing`,
//!   `incoming`, `both`) and a `relationTypes` filter; neighbourhoods are bounded by depth,
//!   entity count and a token budget.
//! * **Diagnostics by id or path**, with source positions when the file is unchanged since
//!   it was indexed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::wiki::diagnostics::{diag, sort_diagnostics, DiagExt};
use crate::wiki::index::{estimate_tokens, EntitySummary, QueryFilter, WikiIndex};
use crate::wiki::maintenance::{inspect_index, observe_corpus, IndexStatus, MAX_DIAGNOSTICS};
use crate::wiki::models::{is_relation_type, WikiDiagnostic};
use crate::wiki::scope::WikiScope;

pub const DEFAULT_PAGE_LIMIT: usize = 25;
pub const MAX_PAGE_LIMIT: usize = 100;
pub const MAX_CURSOR_BYTES: usize = 4096;
pub const MIN_TOKEN_BUDGET: usize = 64;
pub const DEFAULT_TOKEN_BUDGET: usize = 4000;
pub const MAX_NEIGHBORHOOD_ENTITIES: usize = 100;
pub const MAX_RELATIONS_PER_ENTITY: usize = 200;
const MAX_QUERY_CHARS: usize = 256;

/// Session failures are one boxed diagnostic (code, message, remediation).
pub type SessionResult<T> = Result<T, Box<WikiDiagnostic>>;

fn invalid(msg: impl Into<String>) -> Box<WikiDiagnostic> {
    Box::new(diag("INVALID_REQUEST", msg, ""))
}

/// A paged answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractPage<T> {
    pub items: Vec<T>,
    /// More items exist under this exact request and revision.
    pub next_cursor: Option<String>,
    pub estimated_tokens: usize,
    /// A page, safety or token bound omitted content from this answer.
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ListRequest {
    #[serde(alias = "type")]
    pub types: Vec<String>,
    #[serde(alias = "status")]
    pub statuses: Vec<String>,
    pub topic: Option<String>,
    pub include_archived: bool,
    pub limit: Option<usize>,
    pub max_tokens: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchRequest {
    #[serde(alias = "text")]
    pub query: String,
    #[serde(flatten)]
    pub list: ListRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Outgoing,
    Incoming,
    #[default]
    Both,
}

impl Direction {
    pub fn parse(s: &str) -> SessionResult<Self> {
        match s {
            "outgoing" => Ok(Self::Outgoing),
            "incoming" => Ok(Self::Incoming),
            "both" => Ok(Self::Both),
            other => Err(invalid(format!(
                "direction {:?} is not outgoing, incoming or both",
                other
            ))),
        }
    }
    fn outgoing(&self) -> bool {
        *self != Self::Incoming
    }
    fn incoming(&self) -> bool {
        *self != Self::Outgoing
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelationRequest {
    pub entity_id: String,
    pub direction: Direction,
    pub relation_types: Vec<String>,
    pub include_archived: bool,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NeighborhoodRequest {
    pub entity_id: String,
    pub direction: Direction,
    pub relation_types: Vec<String>,
    pub depth: Option<usize>,
    pub max_entities: Option<usize>,
    pub max_tokens: Option<usize>,
    pub include_archived: bool,
}

/// Options of [`WikiReadSession::get_with`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GetRequest {
    pub id: String,
    pub include_body: bool,
    pub limit: Option<usize>,
    pub relations_offset: usize,
    pub backlinks_offset: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DiagnosticRequest {
    pub entity_ids: Vec<String>,
    pub paths: Vec<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContractRelation {
    #[serde(rename = "type")]
    pub rel_type: String,
    pub source_id: String,
    pub target_id: String,
    pub resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationHit {
    pub relation: ContractRelation,
    /// `outgoing` (from the asked entity) or `incoming` (to it).
    pub direction: String,
    /// The entity at the other end (None when unresolved or hidden as archived).
    pub entity: Option<EntitySummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractNeighborhood {
    pub root: EntitySummary,
    pub entities: Vec<EntitySummary>,
    pub relations: Vec<ContractRelation>,
    pub depth: usize,
    pub estimated_tokens: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractGrounding {
    #[serde(rename = "ref")]
    pub reference: String,
    pub origin: String,
    pub health: Option<String>,
    pub state: Option<String>,
    pub body_hash: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadValidation {
    pub valid: bool,
    pub status: IndexStatus,
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Cursor {
    v: u32,
    operation: String,
    indexed_revision: String,
    corpus_revision: String,
    request_hash: String,
    offset: usize,
}

// ---------------------------------------------------------------------------
// base64url (no padding)
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(s.len());
    for c in s.bytes() {
        vals.push(B64.iter().position(|x| *x == c)? as u32);
    }
    if vals.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        let mut n = 0u32;
        for (i, v) in chunk.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        let bytes = chunk.len() - 1;
        for i in 0..bytes {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Canonical JSON: object keys sorted, nulls dropped.
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().filter(|k| !m[k.as_str()].is_null()).collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        Value::String((*k).clone()),
                        canonical_json(&m[k.as_str()])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

fn sha_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

fn page_limit(limit: Option<usize>) -> SessionResult<usize> {
    match limit {
        None => Ok(DEFAULT_PAGE_LIMIT),
        Some(0) => Err(invalid("limit must be at least 1")),
        Some(n) if n > MAX_PAGE_LIMIT => {
            Err(invalid(format!("limit must be at most {}", MAX_PAGE_LIMIT)))
        }
        Some(n) => Ok(n),
    }
}

fn token_budget(max_tokens: Option<usize>) -> SessionResult<usize> {
    match max_tokens {
        None => Ok(DEFAULT_TOKEN_BUDGET),
        Some(n) if n < MIN_TOKEN_BUDGET => Err(invalid(format!(
            "maxTokens must be at least {}",
            MIN_TOKEN_BUDGET
        ))),
        Some(n) => Ok(n),
    }
}

fn check_strings(label: &str, values: &[String]) -> SessionResult<()> {
    for v in values {
        if v.is_empty() || v.len() > MAX_QUERY_CHARS || v.contains('\0') {
            return Err(invalid(format!(
                "{} values must be 1-{} characters",
                label, MAX_QUERY_CHARS
            )));
        }
    }
    Ok(())
}

/// One open read session.
pub struct WikiReadSession {
    index: WikiIndex,
    status: IndexStatus,
    indexed_revision: String,
    corpus_revision: String,
    snapshot_revision: String,
    scope: WikiScope,
}

impl Drop for WikiReadSession {
    fn drop(&mut self) {
        let _ = self.index.connection().execute_batch("COMMIT");
    }
}

/// Open a session over the index at `db_path` for the corpus of `scope`.
pub fn open_read_session(db_path: &Path, scope: &WikiScope) -> SessionResult<WikiReadSession> {
    let status = inspect_index(db_path, scope, true);
    if !status.readable() {
        let first = status.diagnostics.first().cloned().unwrap_or_else(|| {
            diag(
                "WIKI_INDEX_MISSING",
                format!("The wiki index is {}", status.state),
                "wiki.db",
            )
        });
        return Err(Box::new(first));
    }
    let index = WikiIndex::open_read_only(db_path).map_err(|e| {
        diag(
            crate::wiki::index::error_code(&e).unwrap_or("WIKI_INDEX_CORRUPT"),
            e.to_string(),
            "wiki.db",
        )
    })?;
    // Pin one snapshot for every read in this session.
    let conn = index.connection();
    conn.execute_batch("BEGIN DEFERRED")
        .and_then(|_| {
            conn.query_row("SELECT COUNT(*) FROM wiki_files", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .map_err(|e| {
            diag(
                "WIKI_INDEX_CORRUPT",
                format!("Cannot open a read snapshot: {}", e),
                "wiki.db",
            )
        })?;
    let indexed_revision = index
        .indexed_revision()
        .map_err(|e| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db"))?;
    let snapshot_revision = sha_hex(&format!(
        "{}\0{}",
        indexed_revision,
        index.meta_value("last_refresh").unwrap_or_default()
    ));
    let corpus_revision = observe_corpus(scope).revision;
    Ok(WikiReadSession {
        index,
        status,
        indexed_revision,
        corpus_revision,
        snapshot_revision,
        scope: scope.clone(),
    })
}

impl WikiReadSession {
    pub fn status(&self) -> &IndexStatus {
        &self.status
    }
    pub fn indexed_revision(&self) -> &str {
        &self.indexed_revision
    }
    pub fn snapshot_revision(&self) -> &str {
        &self.snapshot_revision
    }
    pub fn corpus_revision(&self) -> &str {
        &self.corpus_revision
    }
    pub fn index(&self) -> &WikiIndex {
        &self.index
    }

    fn encode_cursor(&self, operation: &str, request_hash: &str, offset: usize) -> String {
        let c = Cursor {
            v: 1,
            operation: operation.to_string(),
            indexed_revision: self.indexed_revision.clone(),
            corpus_revision: self.corpus_revision.clone(),
            request_hash: request_hash.to_string(),
            offset,
        };
        b64url_encode(canonical_json(&json!(c)).as_bytes())
    }

    /// Offset named by `cursor` (0 without one), checked against this request and revision.
    fn decode_cursor(
        &self,
        cursor: Option<&str>,
        operation: &str,
        request_hash: &str,
    ) -> SessionResult<usize> {
        let Some(cursor) = cursor.filter(|c| !c.is_empty()) else {
            return Ok(0);
        };
        if cursor.len() > MAX_CURSOR_BYTES {
            return Err(invalid("Cursor is invalid"));
        }
        let parsed: Cursor = b64url_decode(cursor)
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|c: &Cursor| c.v == 1 && c.offset <= 100_000)
            .ok_or_else(|| invalid("Cursor is invalid"))?;
        if parsed.operation != operation || parsed.request_hash != request_hash {
            return Err(invalid("The cursor belongs to a different request; pass the same arguments, or start over without a cursor"));
        }
        if parsed.indexed_revision != self.indexed_revision
            || parsed.corpus_revision != self.corpus_revision
        {
            return Err(Box::new(diag(
                "REVISION_CONFLICT",
                "The wiki changed since this cursor was issued; start over without a cursor",
                "",
            )));
        }
        Ok(parsed.offset)
    }

    fn request_hash<T: Serialize>(req: &T) -> String {
        let mut v = json!(req);
        if let Some(o) = v.as_object_mut() {
            o.remove("cursor");
        }
        sha_hex(&canonical_json(&v))
    }

    fn not_found(id: &str) -> WikiDiagnostic {
        diag("ENTITY_NOT_FOUND", format!("No entity has id {}", id), "").for_entity(id)
    }

    /// One entity with its groundings, backlinks and source location, with the body (and
    /// default paging of relations and backlinks).
    pub fn get(&self, id: &str) -> SessionResult<Value> {
        self.get_with(
            id,
            &GetRequest {
                include_body: true,
                ..Default::default()
            },
        )
    }

    /// One entity, bounded: the body only with `includeBody`; outgoing relations and
    /// backlinks paged by `limit` (default 25, max 200) with `relationsPage` /
    /// `backlinksPage` reporting `total`, `truncated` and `nextOffset`.
    pub fn get_with(&self, id: &str, req: &GetRequest) -> SessionResult<Value> {
        check_strings("id", &[id.to_string()])?;
        if let Some(l) = req.limit {
            if l == 0 || l > crate::wiki::index::MAX_LINK_PAGE {
                return Err(invalid(format!(
                    "limit must be between 1 and {}",
                    crate::wiki::index::MAX_LINK_PAGE
                )));
            }
        }
        let detail = self
            .index
            .entity_detail(
                id,
                &crate::wiki::index::DetailOptions {
                    include_body: req.include_body,
                    include_related: false,
                    limit: req.limit,
                    relations_offset: req.relations_offset,
                    backlinks_offset: req.backlinks_offset,
                    related_offset: 0,
                },
            )
            .map_err(|e| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db"))?
            .ok_or_else(|| Self::not_found(id))?;
        let e = &detail.entity;
        let groundings = self.grounding_status(&e.id)?;
        let backlinks: Vec<Value> = detail
            .backlinks
            .items
            .iter()
            .map(|b| json!({ "id": b.id, "title": b.title, "type": b.entity_type }))
            .collect();
        let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
        if !req.include_body {
            if let Some(o) = v.as_object_mut() {
                o.remove("body");
            }
        }
        v["bodyIncluded"] = json!(req.include_body);
        v["groundings"] = json!(groundings);
        v["backlinks"] = json!(backlinks);
        v["backlinksPage"] = json!({
            "total": detail.backlinks.total,
            "truncated": detail.backlinks.truncated,
            "limit": detail.backlinks.limit,
            "offset": detail.backlinks.offset,
            "nextOffset": detail.backlinks.next_offset,
        });
        v["relationsPage"] = json!(detail.relations_page);
        v["location"] = json!({ "file": e.file, "startLine": e.start_line, "endLine": e.end_line });
        Ok(v)
    }

    fn filter(list: &ListRequest, offset: usize, limit: usize) -> QueryFilter {
        QueryFilter {
            types: list.types.clone(),
            topic: list.topic.clone(),
            statuses: list.statuses.clone(),
            include_archived: list.include_archived,
            limit: Some(limit),
            offset,
            ..Default::default()
        }
    }

    fn bounded<T: Serialize + Clone>(
        &self,
        items: Vec<T>,
        more: bool,
        offset: usize,
        budget: usize,
        operation: &str,
        request_hash: &str,
    ) -> ContractPage<T> {
        let mut out = Vec::new();
        let mut tokens = 0;
        let mut cut = false;
        for item in items {
            let t = estimate_tokens(&item);
            if !out.is_empty() && tokens + t > budget {
                cut = true;
                break;
            }
            tokens += t;
            out.push(item);
        }
        let next =
            (more || cut).then(|| self.encode_cursor(operation, request_hash, offset + out.len()));
        ContractPage {
            truncated: next.is_some(),
            next_cursor: next,
            estimated_tokens: tokens,
            items: out,
        }
    }

    /// Bounded, paged list of entity summaries.
    pub fn list(&self, req: &ListRequest) -> SessionResult<ContractPage<EntitySummary>> {
        check_strings("type", &req.types)?;
        check_strings("status", &req.statuses)?;
        let limit = page_limit(req.limit)?;
        let budget = token_budget(req.max_tokens)?;
        let hash = Self::request_hash(req);
        let offset = self.decode_cursor(req.cursor.as_deref(), "list", &hash)?;
        let page = self
            .index
            .list_filtered(&Self::filter(req, offset, limit))
            .map_err(|e| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db"))?;
        Ok(self.bounded(page.items, page.truncated, offset, budget, "list", &hash))
    }

    /// Ranked, bounded, paged search.
    pub fn search(&self, req: &SearchRequest) -> SessionResult<ContractPage<Value>> {
        if req.query.chars().count() > MAX_QUERY_CHARS || req.query.contains('\0') {
            return Err(invalid(format!(
                "query must be at most {} characters",
                MAX_QUERY_CHARS
            )));
        }
        if req.query.trim().is_empty() {
            let page = self.list(&req.list)?;
            return Ok(ContractPage {
                items: page
                    .items
                    .into_iter()
                    .map(|s| json!({ "entity": s, "matched": Value::Null }))
                    .collect(),
                next_cursor: page.next_cursor,
                estimated_tokens: page.estimated_tokens,
                truncated: page.truncated,
            });
        }
        check_strings("type", &req.list.types)?;
        check_strings("status", &req.list.statuses)?;
        let limit = page_limit(req.list.limit)?;
        let budget = token_budget(req.list.max_tokens)?;
        let hash = Self::request_hash(req);
        let offset = self.decode_cursor(req.list.cursor.as_deref(), "search", &hash)?;
        let page = self
            .index
            .search(&req.query, &Self::filter(&req.list, offset, limit))
            .map_err(|e| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db"))?;
        let items: Vec<Value> = page
            .items
            .into_iter()
            .map(|h| json!({ "entity": h.entity, "matched": h.matched }))
            .collect();
        Ok(self.bounded(items, page.truncated, offset, budget, "search", &hash))
    }

    fn key_of(&self, id: &str) -> SessionResult<Option<String>> {
        self.index
            .connection()
            .query_row(
                "SELECT entity_key FROM wiki_entities WHERE id = ?1 AND shadowed = 0 LIMIT 1",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| Box::new(diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db")))
    }

    /// Every relation touching `id` in `direction`, filtered by type, sorted deterministically.
    fn raw_relations(
        &self,
        id: &str,
        direction: &Direction,
        types: &[String],
    ) -> SessionResult<Vec<(ContractRelation, String)>> {
        let conn = self.index.connection();
        let mut out = Vec::new();
        let err = |e: rusqlite::Error| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db");
        if direction.outgoing() {
            let mut stmt = conn
                .prepare(
                    "SELECT r.type, r.target_id, r.target_resolved FROM wiki_relations r JOIN wiki_entities e ON e.entity_key = r.source_key
                     WHERE e.id = ?1 AND e.shadowed = 0 ORDER BY r.type, r.target_id, r.ordinal LIMIT ?2",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map(params![id, MAX_RELATIONS_PER_ENTITY as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? != 0,
                    ))
                })
                .map_err(err)?;
            for row in rows {
                let (t, target, resolved) = row.map_err(err)?;
                out.push((
                    ContractRelation {
                        rel_type: t,
                        source_id: id.to_string(),
                        target_id: target,
                        resolved,
                    },
                    "outgoing".to_string(),
                ));
            }
        }
        if direction.incoming() {
            let mut stmt = conn
                .prepare(
                    "SELECT r.type, e.id FROM wiki_relations r JOIN wiki_entities e ON e.entity_key = r.source_key
                     WHERE r.target_id = ?1 AND e.shadowed = 0 ORDER BY r.type, e.id, r.ordinal LIMIT ?2",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map(params![id, MAX_RELATIONS_PER_ENTITY as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(err)?;
            for row in rows {
                let (t, source) = row.map_err(err)?;
                out.push((
                    ContractRelation {
                        rel_type: t,
                        source_id: source,
                        target_id: id.to_string(),
                        resolved: true,
                    },
                    "incoming".to_string(),
                ));
            }
        }
        if !types.is_empty() {
            out.retain(|(r, _)| types.contains(&r.rel_type));
        }
        Ok(out)
    }

    fn visible(&self, id: &str, include_archived: bool) -> Option<EntitySummary> {
        self.index
            .summary(id)
            .ok()
            .flatten()
            .filter(|s| include_archived || s.status != "archived")
    }

    fn check_relation_types(types: &[String]) -> SessionResult<()> {
        for t in types {
            if !is_relation_type(t) {
                return Err(invalid(format!(
                    "{:?} is not a registered relation type",
                    t
                )));
            }
        }
        Ok(())
    }

    /// Paged relations of one entity.
    pub fn relations(&self, req: &RelationRequest) -> SessionResult<ContractPage<RelationHit>> {
        check_strings("entityId", std::slice::from_ref(&req.entity_id))?;
        Self::check_relation_types(&req.relation_types)?;
        let limit = page_limit(req.limit)?;
        let hash = Self::request_hash(req);
        let offset = self.decode_cursor(req.cursor.as_deref(), "relations", &hash)?;
        if self.key_of(&req.entity_id)?.is_none() {
            return Err(Box::new(Self::not_found(&req.entity_id)));
        }
        let all = self.raw_relations(&req.entity_id, &req.direction, &req.relation_types)?;
        let total = all.len();
        let items: Vec<RelationHit> = all
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(relation, direction)| {
                let other = if direction == "outgoing" {
                    &relation.target_id
                } else {
                    &relation.source_id
                };
                let entity = self.visible(other, req.include_archived);
                RelationHit {
                    relation,
                    direction,
                    entity,
                }
            })
            .collect();
        let more = offset + items.len() < total;
        Ok(self.bounded(items, more, offset, usize::MAX, "relations", &hash))
    }

    /// Bounded breadth-first neighbourhood.
    pub fn neighborhood(&self, req: &NeighborhoodRequest) -> SessionResult<ContractNeighborhood> {
        check_strings("entityId", std::slice::from_ref(&req.entity_id))?;
        Self::check_relation_types(&req.relation_types)?;
        let depth = match req.depth {
            None => crate::wiki::index::DEFAULT_TRAVERSAL_DEPTH,
            Some(d) if (1..=crate::wiki::index::MAX_TRAVERSAL_DEPTH).contains(&d) => d,
            Some(_) => {
                return Err(invalid(format!(
                    "depth must be 1-{}",
                    crate::wiki::index::MAX_TRAVERSAL_DEPTH
                )))
            }
        };
        let max_entities = match req.max_entities {
            None => 25,
            Some(n) if (1..=MAX_NEIGHBORHOOD_ENTITIES).contains(&n) => n,
            Some(_) => {
                return Err(invalid(format!(
                    "maxEntities must be 1-{}",
                    MAX_NEIGHBORHOOD_ENTITIES
                )))
            }
        };
        let budget = token_budget(req.max_tokens)?;
        let root = self
            .index
            .summary(&req.entity_id)
            .map_err(|e| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db"))?
            .ok_or_else(|| Self::not_found(&req.entity_id))?;
        let mut tokens = estimate_tokens(&root);
        let mut seen: HashSet<String> = HashSet::from([root.id.clone()]);
        let mut entities = Vec::new();
        let mut relations: Vec<ContractRelation> = Vec::new();
        let mut rel_seen: HashSet<(String, String, String)> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::from([(root.id.clone(), 0)]);
        let mut truncated = false;
        'walk: while let Some((id, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for (rel, direction) in self.raw_relations(&id, &req.direction, &req.relation_types)? {
                let other = if direction == "outgoing" {
                    rel.target_id.clone()
                } else {
                    rel.source_id.clone()
                };
                let key = (
                    rel.source_id.clone(),
                    rel.rel_type.clone(),
                    rel.target_id.clone(),
                );
                if seen.contains(&other) {
                    if rel_seen.insert(key) {
                        relations.push(rel);
                    }
                    continue;
                }
                let Some(summary) = self.visible(&other, req.include_archived) else {
                    continue;
                };
                let cost = estimate_tokens(&summary) + estimate_tokens(&rel);
                if entities.len() >= max_entities || tokens + cost > budget {
                    truncated = true;
                    break 'walk;
                }
                tokens += cost;
                seen.insert(other.clone());
                if rel_seen.insert(key) {
                    relations.push(rel);
                }
                entities.push(summary);
                queue.push_back((other, d + 1));
            }
        }
        // Keep only relations between reached entities.
        relations.retain(|r| seen.contains(&r.source_id) && seen.contains(&r.target_id));
        Ok(ContractNeighborhood {
            root,
            entities,
            relations,
            depth,
            estimated_tokens: tokens,
            truncated,
        })
    }

    /// Groundings of one entity with their derived health (None when the id is unknown).
    pub fn grounding_status(&self, id: &str) -> SessionResult<Vec<ContractGrounding>> {
        let Some(key) = self.key_of(id)? else {
            return Err(Box::new(Self::not_found(id)));
        };
        let err = |e: rusqlite::Error| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db");
        let conn = self.index.connection();
        let mut stmt = conn
            .prepare(
                "SELECT node_id, origin, health, state, body_hash, fingerprint FROM wiki_groundings WHERE entity_key = ?1 ORDER BY ordinal",
            )
            .map_err(err)?;
        let rows = stmt
            .query_map(params![key], |r| {
                Ok(ContractGrounding {
                    reference: r.get(0)?,
                    origin: r.get(1)?,
                    health: r.get(2)?,
                    state: r.get(3)?,
                    body_hash: r.get(4)?,
                    fingerprint: r.get(5)?,
                })
            })
            .map_err(err)?;
        Ok(rows.collect::<Result<Vec<_>, _>>().map_err(err)?)
    }

    /// Index-time diagnostics, filtered by entity ids and/or paths, with source positions
    /// when the file is unchanged since it was indexed.
    pub fn diagnostics(
        &self,
        req: &DiagnosticRequest,
    ) -> SessionResult<ContractPage<WikiDiagnostic>> {
        check_strings("entityIds", &req.entity_ids)?;
        check_strings("paths", &req.paths)?;
        let limit = req
            .limit
            .unwrap_or(MAX_DIAGNOSTICS)
            .clamp(1, MAX_DIAGNOSTICS);
        let all = self
            .index
            .stored_diagnostics(100_000)
            .map_err(|e| diag("WIKI_INDEX_CORRUPT", e.to_string(), "wiki.db"))?;
        let mut matching: Vec<WikiDiagnostic> = all
            .into_iter()
            .filter(|d| {
                (req.entity_ids.is_empty()
                    || d.entity_id
                        .as_ref()
                        .is_some_and(|e| req.entity_ids.contains(e)))
                    && (req.paths.is_empty()
                        || req.paths.iter().any(|p| {
                            d.file == *p
                                || d.file.starts_with(&format!("{}/", p.trim_end_matches('/')))
                        }))
            })
            .collect();
        sort_diagnostics(&mut matching);
        let total = matching.len();
        matching.truncate(limit);
        self.locate(&mut matching);
        Ok(ContractPage {
            truncated: total > matching.len(),
            next_cursor: None,
            estimated_tokens: estimate_tokens(&matching),
            items: matching,
        })
    }

    /// Positions for stored diagnostics, from files whose bytes still match the index.
    fn locate(&self, diags: &mut [WikiDiagnostic]) {
        let mut files: HashMap<String, Option<crate::wiki::parser::ParsedFile>> = HashMap::new();
        for d in diags.iter_mut() {
            if d.file.is_empty() {
                continue;
            }
            let parsed = files.entry(d.file.clone()).or_insert_with(|| {
                let text = self.scope.read(&d.file)?;
                let indexed: Option<String> = self
                    .index
                    .connection()
                    .query_row(
                        "SELECT content_hash FROM wiki_files WHERE path = ?1",
                        params![d.file],
                        |r| r.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                if indexed.as_deref()
                    != Some(crate::graph::fingerprint::compute_file_hash(text.as_bytes()).as_str())
                {
                    return None;
                }
                Some(crate::wiki::parser::parse_markdown_file_with(
                    &d.file,
                    &text,
                    &self.scope.registry,
                ))
            });
            if let Some(f) = parsed {
                crate::wiki::validate::locate_diagnostics(
                    std::slice::from_mut(d),
                    std::slice::from_ref(f),
                );
            }
        }
    }

    /// Whether the index is readable and free of error-severity diagnostics.
    pub fn validate(&self) -> ReadValidation {
        let stored = self.index.stored_diagnostics(100_000).unwrap_or_default();
        let errors = stored.iter().filter(|d| d.severity == "error").count();
        let warnings = stored.iter().filter(|d| d.severity == "warning").count();
        ReadValidation {
            valid: errors == 0 && self.status.state == "fresh",
            status: self.status.clone(),
            errors,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips() {
        for s in ["", "a", "ab", "abc", "abcd", "{\"k\":1}"] {
            let e = b64url_encode(s.as_bytes());
            assert!(e.bytes().all(|b| B64.contains(&b)));
            assert_eq!(b64url_decode(&e).unwrap(), s.as_bytes());
        }
        assert!(b64url_decode("a").is_none());
        assert!(b64url_decode("a+").is_none());
    }
}
