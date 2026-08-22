//! Reconciler tier 2: decide whether a grounded symbol that no longer resolves MOVED (and where),
//! is GONE, or is AMBIGUOUS, from MinHash body similarity (LSH candidate lookup over
//! `node_lsh`) and caller/callee neighbourhood continuity.
//!
//! Algorithm:
//! 1. baseline token count < `MIN_TOKENS` → small-node path (step 5).
//! 2. candidates = LSH lookup of the baseline sketch; none → GONE.
//! 3. score(c) = `W_BODY` · minhash similarity + `W_NBR` · neighbour Jaccard.
//! 4. best ≥ `HI`: MOVED unless the runner-up is within `MOVED_MARGIN` (then AMBIGUOUS, unless
//!    exactly one near-tie has the committed body hash); best < `LO` → GONE; else AMBIGUOUS.
//! 5. small nodes: candidates are same-kind nodes sharing ≥ `NBR_MIN_SHARED` neighbours that
//!    are *strong* (neighbour Jaccard ≥ `NBR_HI`) or *compatible* (body ≥ `SMALL_BODY_MIN` and
//!    token-count slack ≤ `SMALL_TOKEN_SLACK`). MOVED needs the best strong and compatible and
//!    alone within the margin; otherwise AMBIGUOUS (or MOVED by an identical body hash).

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};

use crate::graph::engine::CALL_EDGE_KINDS;
use crate::graph::fingerprint::{neighbor_overlap, MinHash, LSH_BANDS};

pub const HI: f64 = 0.85;
pub const LO: f64 = 0.55;
pub const MOVED_MARGIN: f64 = 0.08;
pub const W_BODY: f64 = 0.7;
pub const W_NBR: f64 = 0.3;
pub const MIN_TOKENS: usize = 30;
pub const NBR_MIN_SHARED: usize = 2;
pub const NBR_HI: f64 = 0.8;
pub const SMALL_BODY_MIN: f64 = 0.6;
pub const SMALL_TOKEN_SLACK: f64 = 0.25;
pub const NBR_CANDIDATE_LIMIT: usize = 64;

/// What decided a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Body,
    Neighbors,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Moved { node_id: String, score: f64, evidence: Evidence },
    Ambiguous { candidate: String, score: f64, evidence: Evidence },
    Gone,
}

/// A grounded symbol's fingerprint as recorded with its baseline.
#[derive(Debug, Clone)]
pub struct BaselineFingerprint {
    pub minhash: MinHash,
    /// Sorted caller + callee node ids at baseline time.
    pub neighbors: Vec<String>,
    pub body_hash: Option<String>,
    /// Kind of the missing node (`function`, `method`, ...), when known.
    pub kind: Option<String>,
}

fn node_kind(id: &str) -> Option<&str> {
    id.split_once(':').map(|(k, _)| k).filter(|k| !k.is_empty())
}

fn call_kinds_sql() -> String {
    CALL_EDGE_KINDS.iter().map(|k| format!("'{}'", k)).collect::<Vec<_>>().join(", ")
}

/// Sorted caller + callee ids of a node in the current graph.
pub fn neighbors_of(conn: &Connection, id: &str) -> Vec<String> {
    let sql = format!(
        "SELECT target FROM edges WHERE source = ?1 AND kind IN ({k}) \
         UNION SELECT source FROM edges WHERE target = ?1 AND kind IN ({k})",
        k = call_kinds_sql()
    );
    let mut out: Vec<String> = conn
        .prepare_cached(&sql)
        .and_then(|mut s| s.query_map(params![id], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect()))
        .unwrap_or_default();
    out.retain(|n| n != id);
    out.sort();
    out.dedup();
    out
}

/// MinHash sketch of a node in the current graph (`node_minhash`), when it has one.
pub fn node_fingerprint(conn: &Connection, id: &str) -> Option<MinHash> {
    fingerprint_of(conn, id)
}

fn fingerprint_of(conn: &Connection, id: &str) -> Option<MinHash> {
    conn.query_row(
        "SELECT minhash, token_count FROM node_minhash WHERE node_id = ?1",
        params![id],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
    .map(|(b, t)| MinHash::from_blob(&b, t as usize))
}

fn body_hash_of(conn: &Connection, id: &str) -> Option<String> {
    conn.query_row("SELECT body_hash FROM nodes WHERE id = ?1", params![id], |r| r.get::<_, Option<String>>(0))
        .optional()
        .ok()
        .flatten()
        .flatten()
}

/// Nodes sharing at least one LSH band bucket with `mh`.
fn lsh_candidates(conn: &Connection, mh: &MinHash) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let Ok(mut stmt) = conn.prepare_cached("SELECT node_id FROM node_lsh WHERE band = ?1 AND bucket = ?2") else {
        return Vec::new();
    };
    for (band, bucket) in mh.band_hashes().into_iter().enumerate().take(LSH_BANDS) {
        if let Ok(rows) = stmt.query_map(params![band as i64, bucket], |r| r.get::<_, String>(0)) {
            out.extend(rows.flatten());
        }
    }
    out.into_iter().collect()
}

/// Nodes adjacent (by call edges) to at least `min_shared` of `neighbors`, most shared first.
fn neighborhood(conn: &Connection, neighbors: &[String], min_shared: usize, limit: usize) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in neighbors {
        for adj in neighbors_of(conn, n) {
            *counts.entry(adj).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().filter(|(_, c)| *c >= min_shared).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(limit).map(|(id, _)| id).collect()
}

struct Scored {
    id: String,
    score: f64,
    strong: bool,
    compatible: bool,
}

fn sort_scored(v: &mut [Scored]) {
    v.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id)));
}

/// The best candidate and every other within `MOVED_MARGIN` of it.
fn near_ties(scored: &[Scored]) -> Vec<&Scored> {
    match scored.first() {
        Some(best) => scored.iter().filter(|c| best.score - c.score < MOVED_MARGIN).collect(),
        None => Vec::new(),
    }
}

/// The one same-kind candidate whose body hash equals the committed one, when exactly one does.
fn identical_text(conn: &Connection, kind: Option<&str>, ids: &[&str], body_hash: Option<&str>) -> Option<String> {
    let (Some(kind), Some(hash)) = (kind, body_hash.filter(|h| !h.is_empty())) else {
        return None;
    };
    let matches: Vec<&&str> = ids
        .iter()
        .filter(|id| node_kind(id) == Some(kind) && body_hash_of(conn, id).as_deref() == Some(hash))
        .collect();
    (matches.len() == 1).then(|| matches[0].to_string())
}

/// Reconcile a missing grounded symbol against the current graph. `exclude` is the missing
/// node's own id (never a candidate).
pub fn reconcile(conn: &Connection, baseline: &BaselineFingerprint, exclude: Option<&str>) -> Verdict {
    let kind = baseline.kind.as_deref();
    let same_kind = |id: &str| kind.is_none_or(|k| crate::graph::grounding::kinds_equivalent(k, node_kind(id).unwrap_or("")));
    if baseline.minhash.token_count < MIN_TOKENS {
        return reconcile_small(conn, baseline, exclude);
    }
    let mut scored: Vec<Scored> = lsh_candidates(conn, &baseline.minhash)
        .into_iter()
        .filter(|id| Some(id.as_str()) != exclude && same_kind(id))
        .filter_map(|id| {
            let fp = fingerprint_of(conn, &id)?;
            let nbrs = neighbors_of(conn, &id);
            let score = W_BODY * baseline.minhash.similarity(&fp) + W_NBR * neighbor_overlap(&baseline.neighbors, &nbrs);
            Some(Scored { id, score, strong: false, compatible: false })
        })
        .collect();
    if scored.is_empty() {
        return Verdict::Gone;
    }
    sort_scored(&mut scored);
    let best = &scored[0];
    if best.score >= HI {
        if scored.len() > 1 && best.score - scored[1].score < MOVED_MARGIN {
            let ties: Vec<&str> = near_ties(&scored).iter().map(|s| s.id.as_str()).collect();
            return match identical_text(conn, kind, &ties, baseline.body_hash.as_deref()) {
                Some(id) => Verdict::Moved { node_id: id, score: best.score, evidence: Evidence::Body },
                None => Verdict::Ambiguous { candidate: best.id.clone(), score: best.score, evidence: Evidence::Body },
            };
        }
        return Verdict::Moved { node_id: best.id.clone(), score: best.score, evidence: Evidence::Body };
    }
    if best.score < LO {
        return Verdict::Gone;
    }
    Verdict::Ambiguous { candidate: best.id.clone(), score: best.score, evidence: Evidence::Body }
}

fn reconcile_small(conn: &Connection, baseline: &BaselineFingerprint, exclude: Option<&str>) -> Verdict {
    let kind = baseline.kind.as_deref();
    let mut scored: Vec<Scored> = neighborhood(conn, &baseline.neighbors, NBR_MIN_SHARED, NBR_CANDIDATE_LIMIT)
        .into_iter()
        .filter(|id| Some(id.as_str()) != exclude)
        .filter(|id| kind.is_some_and(|k| crate::graph::grounding::kinds_equivalent(k, node_kind(id).unwrap_or(""))))
        .filter_map(|id| {
            let fp = fingerprint_of(conn, &id)?;
            let nbr = neighbor_overlap(&baseline.neighbors, &neighbors_of(conn, &id));
            let body = baseline.minhash.similarity(&fp);
            let slack = (fp.token_count as f64 - baseline.minhash.token_count as f64).abs()
                / (baseline.minhash.token_count.max(1) as f64);
            let strong = nbr >= NBR_HI;
            let compatible = body >= SMALL_BODY_MIN && slack <= SMALL_TOKEN_SLACK;
            (strong || compatible).then_some(Scored { id, score: W_BODY * body + W_NBR * nbr, strong, compatible })
        })
        .collect();
    sort_scored(&mut scored);
    let Some(best) = scored.first() else {
        let lsh: Vec<String> = lsh_candidates(conn, &baseline.minhash)
            .into_iter()
            .filter(|id| Some(id.as_str()) != exclude)
            .collect();
        let ids: Vec<&str> = lsh.iter().map(String::as_str).collect();
        return match identical_text(conn, kind, &ids, baseline.body_hash.as_deref()) {
            Some(id) => Verdict::Ambiguous { candidate: id, score: 0.0, evidence: Evidence::Body },
            None => Verdict::Gone,
        };
    };
    let close = near_ties(&scored);
    if close.len() == 1 && best.strong && best.compatible {
        return Verdict::Moved { node_id: best.id.clone(), score: best.score, evidence: Evidence::Neighbors };
    }
    let strong_ties: Vec<&str> = close.iter().filter(|c| c.strong).map(|c| c.id.as_str()).collect();
    match identical_text(conn, kind, &strong_ties, baseline.body_hash.as_deref()) {
        Some(id) => Verdict::Moved { node_id: id, score: best.score, evidence: Evidence::Body },
        None => Verdict::Ambiguous { candidate: best.id.clone(), score: best.score, evidence: Evidence::Neighbors },
    }
}

/// Neighbours recorded with a grounding baseline (empty when none were recorded).
pub fn baseline_neighbors(conn: &Connection, subject_kind: &str, doc: &str, reference: &str) -> Vec<String> {
    conn.query_row(
        "SELECT neighbors FROM _knobyte_grounded_neighbors WHERE subject_kind = ?1 AND subject_id = ?2 AND node_id = ?3",
        params![subject_kind, doc, reference],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
    .unwrap_or_default()
}
