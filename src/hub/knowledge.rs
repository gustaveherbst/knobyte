//! Knowledge-page panels: the old-versus-new drift panel, the supersession
//! timeline and the evidence panel (sources, provenance, grounding health and
//! traceability) of one wiki entity.
//!
//! Every read is read-only: the wiki index and the code graph are opened with
//! `SQLITE_OPEN_READ_ONLY` and nothing is rebuilt, migrated or recorded.

use std::collections::{BTreeSet, HashMap, HashSet};

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use super::problem::Query;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{json, Value};

use super::diff::line_diff;
use super::problem::Problem;
use super::HubState;
use crate::config::KnobyteConfig;
use crate::drift::freshness::{inspect_engine, GraphState};
use crate::drift::grounding::{check_groundings, GroundingDoc};
use crate::drift::types::{project_relative, DriftIssue};
use crate::graph::grounding::{
    get_baseline, read_node_source, resolve_baseline, resolve_grounding_ref, scaffold_markdown_files, CommittedIndex,
    RefResolution,
};
use crate::graph::{GraphEngine, Node};
use crate::wiki::index::{doc_ref_of, health_rank, EntitySummary};
use crate::wiki::models::GROUNDING_ORIGIN_ANCHOR;
use crate::wiki::{WikiEntity, WikiIndex};

/// Longest source text (per side) returned by the drift panel.
const MAX_SOURCE_BYTES: usize = 64 * 1024;
/// Longest supersession chain walked.
pub const MAX_TIMELINE_ENTRIES: usize = 100;
/// Relation type that records what an entity replaced.
const SUPERSEDES: &str = "supersedes";

#[derive(Debug, Deserialize)]
pub struct EntityQuery {
    id: String,
}

async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Problem::internal("Hub worker failed").into_response())
}

fn open_wiki(config: &KnobyteConfig) -> Result<WikiIndex, Problem> {
    let path = config.wiki_db_path();
    if !path.exists() {
        return Err(Problem::not_found("Wiki index not built yet"));
    }
    WikiIndex::open_read_only(&path).map_err(|e| Problem::internal(e.to_string()))
}

fn load_entity(wiki: &WikiIndex, id: &str) -> Result<WikiEntity, Problem> {
    match wiki.show(id) {
        Ok(Some(e)) => Ok(e),
        Ok(None) => Err(Problem::not_found(format!("Entity '{}' not found", id))),
        Err(e) => Err(Problem::internal(e.to_string())),
    }
}

fn graph_conn(config: &KnobyteConfig) -> Option<Connection> {
    let path = config.graph_db_path();
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
}

fn bounded(text: String) -> (String, bool) {
    if text.len() <= MAX_SOURCE_BYTES {
        return (text, false);
    }
    let mut cut = MAX_SOURCE_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_string(), true)
}

fn symbol_json(n: &Node) -> Value {
    json!({
        "id": n.id,
        "name": n.name,
        "qualifiedName": n.qualified_name,
        "kind": n.kind,
        "filePath": n.file_path,
        "startLine": n.start_line,
        "endLine": n.end_line,
    })
}

fn issue_json(i: &DriftIssue) -> Value {
    json!({
        "code": i.code,
        "severity": i.severity,
        "message": i.message,
        "line": i.line,
        "candidate": i.candidate,
    })
}

/// Grounding issues the drift checker reports for `entity`'s references (read-only; every
/// scaffold document is read so anchor baselines resolve as `knobyte check` resolves them).
fn grounding_issues(config: &KnobyteConfig, entity: &WikiEntity, engine: Option<&GraphEngine>) -> (Vec<DriftIssue>, Value) {
    let freshness = match engine {
        Some(e) => inspect_engine(e, &config.project_root),
        None => crate::drift::freshness::inspect_graph(config),
    };
    let graph = json!({
        "status": freshness.status.as_str(),
        "summary": freshness.summary(),
        "remediation": freshness.remediation,
    });
    let files: Vec<(String, std::path::PathBuf, String)> = scaffold_markdown_files(&config.scaffold_root)
        .into_iter()
        .filter_map(|(rel, path)| std::fs::read_to_string(&path).ok().map(|c| (rel, path, c)))
        .collect();
    let docs: Vec<GroundingDoc> = files
        .iter()
        .map(|(rel, path, content)| GroundingDoc { scaffold_rel: rel, path, content })
        .collect();
    let (issues, _) = check_groundings(config, &docs, engine.map(|e| e.connection()), &freshness);
    let source = project_relative(&config.project_root, &config.scaffold_root.join(&entity.file));
    let refs: HashSet<&str> = entity.grounds_to.iter().map(String::as_str).collect();
    let (start, end) = (entity.start_line, entity.end_line.max(entity.start_line));
    let mine = issues
        .into_iter()
        .filter(|i| i.file == source)
        .filter(|i| i.symbol.as_deref().is_some_and(|s| refs.contains(s)))
        // An anchor issue carries its line: keep the ones inside this entity's span.
        .filter(|i| match i.line {
            Some(l) if start > 0 => l >= start && l <= end,
            _ => true,
        })
        .collect();
    (mine, graph)
}

/// Old-versus-new panes for every grounding of `entity`.
pub fn drift_panel(config: &KnobyteConfig, wiki: &WikiIndex, entity: &WikiEntity) -> Value {
    let engine = if config.graph_db_path().exists() {
        GraphEngine::open_read_only(&config.graph_db_path()).ok()
    } else {
        None
    };
    let conn = graph_conn(config);
    let health: HashMap<String, (Option<String>, Option<String>)> = wiki
        .groundings_for(&entity.id)
        .unwrap_or_default()
        .into_iter()
        .map(|(r, h, s)| (r, (h, s)))
        .collect();
    let (issues, graph) = grounding_issues(config, entity, engine.as_ref());
    let committed = CommittedIndex::from_scaffold(&config.scaffold_root);
    let stale = graph["status"] != GraphState::Fresh.as_str();

    // The committed baselines come from the Markdown as it is on disk now (a teammate's
    // re-baseline arrives with a pull, before the wiki index is rebuilt).
    let on_disk: Option<WikiEntity> = std::fs::read_to_string(config.scaffold_root.join(&entity.file))
        .ok()
        .and_then(|text| {
            crate::wiki::parser::parse_markdown_file(&entity.file, &text)
                .entities
                .into_iter()
                .map(|pe| pe.entity)
                .find(|e| e.id == entity.id)
        });
    let current_entity = on_disk.as_ref().unwrap_or(entity);

    let mut panes = Vec::new();
    let mut codes: BTreeSet<String> = BTreeSet::new();
    for reference in &entity.grounds_to {
        let dr = doc_ref_of(current_entity, reference);
        let baseline = resolve_baseline(conn.as_ref(), &entity.file, &dr, &committed);
        let node = conn.as_ref().and_then(|c| match resolve_grounding_ref(c, reference) {
            Ok(RefResolution::Resolved(n)) => Some(*n),
            _ => None,
        });
        // The locally cached old source belongs to the cached baseline: show it only when
        // that is the committed baseline, never under another baseline's hash.
        let cached_hash = conn
            .as_ref()
            .and_then(|c| get_baseline(c, &entity.file, reference))
            .map(|b| b.body_hash)
            .filter(|h| !h.is_empty());
        let source_matches = match (&baseline.body_hash, &cached_hash) {
            (Some(b), Some(c)) => b == c,
            (None, _) => true,
            (Some(_), None) => !baseline.committed,
        };
        let old_source_note = (baseline.source.is_some() && !source_matches).then(|| {
            "The committed baseline differs from the one cached in this checkout, so its old source is not available locally; the current source is shown. Run `knobyte graph refresh` (or `knobyte setup`) after pulling to cache it.".to_string()
        });
        let (old_source, old_truncated) = match baseline.source.clone().filter(|_| source_matches) {
            Some(s) => {
                let (s, t) = bounded(s);
                (Some(s), t)
            }
            None => (None, false),
        };
        let (new_source, new_truncated) = match node.as_ref().and_then(|n| read_node_source(&config.project_root, n)) {
            Some(s) => {
                let (s, t) = bounded(s);
                (Some(s), t)
            }
            None => (None, false),
        };
        let current_hash = node.as_ref().and_then(|n| n.body_hash.clone()).filter(|h| !h.is_empty());
        let (h, state) = health.get(reference).cloned().unwrap_or((None, None));
        let pane_issues: Vec<&DriftIssue> = issues.iter().filter(|i| i.symbol.as_deref() == Some(reference)).collect();
        for i in &pane_issues {
            codes.insert(i.code.clone());
        }
        let hash_changed = match (&baseline.body_hash, &current_hash) {
            (Some(b), Some(c)) => b != c,
            _ => false,
        };
        let text_changed = matches!((&old_source, &new_source), (Some(o), Some(n)) if o != n);
        let drifted = conn.is_some() && node.is_none()
            || hash_changed
            || text_changed
            || matches!(h.as_deref(), Some("changed" | "missing"))
            || pane_issues.iter().any(|i| i.code == "GROUNDING_DRIFT" || i.code == "GROUNDING_GONE");
        let diff = match (&old_source, &new_source) {
            (Some(o), Some(n)) if o != n => Some(line_diff(o, n)),
            _ => None,
        };
        panes.push(json!({
            "ref": reference,
            "origin": if matches!(dr.origin, crate::graph::grounding::RefOrigin::Anchor(_)) { GROUNDING_ORIGIN_ANCHOR } else { "frontmatter" },
            "health": h,
            "state": state,
            "resolved": node.is_some(),
            "symbol": node.as_ref().map(symbol_json),
            "baseline": {
                "committed": baseline.committed,
                "bodyHash": baseline.body_hash,
                "source": old_source,
                "truncated": old_truncated,
                "sourceAvailable": old_source.is_some(),
                "sourceNote": old_source_note,
                "nodeId": baseline.node_id,
            },
            "current": {
                "bodyHash": current_hash,
                "source": new_source,
                "truncated": new_truncated,
            },
            "drifted": drifted,
            "diff": diff,
            "issues": pane_issues.iter().map(|i| issue_json(i)).collect::<Vec<_>>(),
        }));
    }
    let drifted = panes.iter().filter(|p| p["drifted"] == true).count();
    json!({
        "entityId": entity.id,
        "file": entity.file,
        "graph": graph,
        "graphStale": stale,
        // Without a graph nothing could be compared: never report that as "no drift".
        "unavailable": conn.is_none(),
        "panes": panes,
        "drifted": drifted,
        "codes": codes,
        "actions": {
            "syncPreview": { "method": "POST", "path": "/api/drift/sync", "body": { "dryRun": true } },
        },
    })
}

fn entity_date(e: &WikiEntity) -> Option<String> {
    let meta = e.metadata.as_ref();
    for key in ["date", "decided_at", "decidedAt", "created_at", "createdAt"] {
        if let Some(v) = meta.and_then(|m| m.get(key)).and_then(Value::as_str) {
            return Some(v.to_string());
        }
    }
    e.provenance.as_ref().and_then(|p| p.created_at.clone())
}

fn supersedes_target(e: &WikiEntity) -> Option<String> {
    e.relations.iter().find(|r| r.rel_type == SUPERSEDES).map(|r| r.target_id.clone())
}

/// The supersession chain through `origin`, oldest first.
/// Order comes from the `supersedes` edges alone; a cycle is reported, never walked forever.
pub fn supersession_timeline(wiki: &WikiIndex, origin: &WikiEntity) -> Value {
    let mut chain: HashMap<String, WikiEntity> = HashMap::new();
    // id -> what it supersedes (None: nothing, or a target outside the index).
    let mut supersedes: HashMap<String, Option<String>> = HashMap::new();
    let mut truncated = false;
    let mut queue: Vec<String> = vec![origin.id.clone()];
    chain.insert(origin.id.clone(), origin.clone());
    let mut visited: HashSet<String> = HashSet::new();
    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        let Some(e) = chain.get(&id).cloned() else { continue };
        // Down: what this entity supersedes.
        let target = supersedes_target(&e);
        supersedes.insert(id.clone(), target.clone());
        if let Some(t) = target {
            if !chain.contains_key(&t) {
                if chain.len() >= MAX_TIMELINE_ENTRIES {
                    truncated = true;
                } else if let Ok(Some(te)) = wiki.show(&t) {
                    chain.insert(t.clone(), te);
                    queue.push(t);
                }
            }
        }
        // Up: what supersedes this entity.
        for b in wiki.backlinks(&id).unwrap_or_default() {
            if supersedes_target(&b).as_deref() != Some(id.as_str()) || chain.contains_key(&b.id) {
                continue;
            }
            if chain.len() >= MAX_TIMELINE_ENTRIES {
                truncated = true;
                break;
            }
            queue.push(b.id.clone());
            chain.insert(b.id.clone(), b);
        }
    }

    // superseded_by: reverse of the in-chain edges (several successors are a fork: first wins,
    // the rest are reported).
    let mut superseded_by: HashMap<String, String> = HashMap::new();
    let mut forks: Vec<Value> = Vec::new();
    let mut ids: Vec<&String> = chain.keys().collect();
    ids.sort();
    for id in &ids {
        if let Some(Some(t)) = supersedes.get(*id) {
            if chain.contains_key(t) {
                if let Some(prev) = superseded_by.get(t) {
                    forks.push(json!({ "id": t, "successors": [prev, id] }));
                } else {
                    superseded_by.insert(t.clone(), (*id).clone());
                }
            }
        }
    }

    // Cycles: following `supersedes` from any member returns to it.
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let mut in_cycle: HashSet<String> = HashSet::new();
    for id in &ids {
        if in_cycle.contains(*id) {
            continue;
        }
        let mut path = vec![(*id).clone()];
        let mut cur = (*id).clone();
        while let Some(Some(next)) = supersedes.get(&cur) {
            if !chain.contains_key(next) || path.len() > chain.len() {
                break;
            }
            if next == *id {
                in_cycle.extend(path.iter().cloned());
                cycles.push(path.clone());
                break;
            }
            if path.contains(next) {
                break;
            }
            path.push(next.clone());
            cur = next.clone();
        }
    }

    // Oldest: a member that supersedes nothing inside the chain.
    let oldest = ids
        .iter()
        .find(|id| match supersedes.get(**id) {
            Some(Some(t)) => !chain.contains_key(t),
            _ => true,
        })
        .map(|s| (*s).clone());
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = oldest;
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break;
        }
        let e = &chain[&id];
        let next = superseded_by.get(&id).cloned();
        entries.push(json!({
            "ordinal": entries.len(),
            "entity": EntitySummary::from(e),
            "status": e.status,
            "lifecycle": e.status,
            "date": entity_date(e),
            "createdAt": e.provenance.as_ref().and_then(|p| p.created_at.clone()),
            "lastModifiedAt": e.provenance.as_ref().and_then(|p| p.last_modified_at.clone()),
            "revision": e.revision,
            "supersedes": supersedes.get(&id).cloned().flatten(),
            "supersededBy": next,
            "current": next.is_none(),
            "origin": id == origin.id,
        }));
        cursor = next;
    }
    json!({
        "entityId": origin.id,
        "entries": entries,
        "cycles": cycles,
        "forks": forks,
        "truncated": truncated,
    })
}

/// Sources, provenance, per-grounding health and traceability of `entity`.
pub fn evidence(config: &KnobyteConfig, wiki: &WikiIndex, entity: &WikiEntity) -> Value {
    let rows = wiki.groundings_for(&entity.id).unwrap_or_default();
    let groundings: Vec<Value> = rows
        .iter()
        .map(|(r, h, s)| {
            let c = entity.committed_for(r);
            json!({
                "ref": r,
                "health": h,
                "state": s,
                "origin": c.map(|c| c.origin.clone()).unwrap_or_else(|| "frontmatter".into()),
                "committedBaseline": c.is_some_and(|c| c.body_hash.is_some()),
            })
        })
        .collect();
    // Null (not "unverified") when nothing was checked: the two are different facts.
    let health = rows
        .iter()
        .filter(|(_, _, s)| s.as_deref() != Some("unchecked"))
        .filter_map(|(_, h, _)| h.clone())
        .max_by_key(|h| health_rank(Some(h)));
    let trace = crate::wiki::trace::trace(wiki, &entity.id, &config.graph_db_path())
        .ok()
        .flatten()
        .and_then(|t| serde_json::to_value(t).ok());
    json!({
        "entity": EntitySummary::from(entity),
        "sources": entity.sources,
        "provenance": entity.provenance,
        "groundings": groundings,
        "health": health,
        "traceability": trace,
    })
}

fn with_entity(state: HubState, id: String, f: impl FnOnce(&KnobyteConfig, &WikiIndex, &WikiEntity) -> Value) -> Response {
    let config = &state.config;
    let wiki = match open_wiki(config) {
        Ok(w) => w,
        Err(p) => return p.into_response(),
    };
    match load_entity(&wiki, &id) {
        Ok(e) => Json(f(config, &wiki, &e)).into_response(),
        Err(p) => p.into_response(),
    }
}

/// `GET /api/wiki/entity/drift?id=`
pub async fn entity_drift(State(state): State<HubState>, Query(q): Query<EntityQuery>) -> Response {
    blocking(move || with_entity(state, q.id, drift_panel)).await
}

/// `GET /api/wiki/entity/timeline?id=`
pub async fn entity_timeline(State(state): State<HubState>, Query(q): Query<EntityQuery>) -> Response {
    blocking(move || with_entity(state, q.id, |_, w, e| supersession_timeline(w, e))).await
}

/// `GET /api/wiki/entity/evidence?id=`
pub async fn entity_evidence(State(state): State<HubState>, Query(q): Query<EntityQuery>) -> Response {
    blocking(move || with_entity(state, q.id, evidence)).await
}
