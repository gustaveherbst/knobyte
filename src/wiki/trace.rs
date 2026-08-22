//! Spec-driven-development traceability: spec → requirement (derived_from) → decision
//! (implements) → component (implements) → implementation (grounding) → test (callers in test
//! files), plus acceptance criteria (`verified_by`) and constraints (`constrained_by`).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::graph::grounding::{resolve_grounding_ref, RefResolution};
use crate::wiki::index::{EntitySummary, WikiIndex};
use crate::wiki::models::{ACCEPTANCE_CRITERION_RELATION, CONSTRAINT_RELATION, SDD_CHAIN};

pub fn is_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let name = p.rsplit('/').next().unwrap_or(&p);
    p.contains("__tests__/")
        || p.starts_with("test/")
        || p.starts_with("tests/")
        || p.contains("/test/")
        || p.contains("/tests/")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.go")
        || name.starts_with("test_")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainNode {
    pub entity: EntitySummary,
    pub upstream: Vec<String>,
    pub downstream: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub entity_id: String,
    pub hop: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Traceability {
    pub origin: EntitySummary,
    pub nodes: BTreeMap<String, Vec<ChainNode>>,
    pub implementations: Vec<(String, String, Option<String>)>,
    pub tests: Vec<(String, String, String)>,
    pub acceptance_criteria: Vec<(String, String, String)>,
    pub constraints: Vec<(String, String, String)>,
    pub gaps: Vec<Gap>,
}

pub fn trace(
    index: &WikiIndex,
    id: &str,
    graph_db: &Path,
) -> rusqlite::Result<Option<Traceability>> {
    let Some(origin) = index.summary(id)? else {
        return Ok(None);
    };
    let mut nodes: BTreeMap<String, Vec<ChainNode>> = BTreeMap::new();
    let mut gaps = Vec::new();
    let mut collected: Vec<EntitySummary> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut frontier: VecDeque<EntitySummary> = VecDeque::from([origin.clone()]);
    while let Some(e) = frontier.pop_front() {
        if !visited.insert(e.id.clone()) {
            continue;
        }
        let n = index
            .neighborhood(&e.id, Some(1), Some(usize::MAX / 4), Some(500), true)?
            .expect("entity exists");
        let mut up = Vec::new();
        let mut down = Vec::new();
        for (from, to, rel) in SDD_CHAIN {
            if e.entity_type == from {
                let reached: Vec<EntitySummary> = n
                    .relations
                    .iter()
                    .filter(|r| r.rel_type == rel)
                    .filter_map(|r| r.target.clone())
                    .filter(|t| t.entity_type == to)
                    .collect();
                if reached.is_empty() {
                    gaps.push(Gap {
                        entity_id: e.id.clone(),
                        hop: format!("{} → {}", from, to),
                        reason: format!("No `{}` relation to a {} entity", rel, to),
                    });
                }
                up.extend(reached);
            }
            if e.entity_type == to {
                down.extend(
                    n.backlinks
                        .iter()
                        .filter(|r| r.rel_type == rel)
                        .filter_map(|r| r.target.clone())
                        .filter(|t| t.entity_type == from),
                );
            }
        }
        for r in up.iter().chain(down.iter()) {
            if !visited.contains(&r.id) {
                frontier.push_back(r.clone());
            }
        }
        nodes
            .entry(e.entity_type.clone())
            .or_default()
            .push(ChainNode {
                entity: e.clone(),
                upstream: up.iter().map(|x| x.id.clone()).collect(),
                downstream: down.iter().map(|x| x.id.clone()).collect(),
            });
        collected.push(e);
    }

    let graph = if graph_db.exists() {
        Connection::open_with_flags(graph_db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
    } else {
        None
    };
    let mut implementations = Vec::new();
    let mut tests = Vec::new();
    let mut acceptance = Vec::new();
    let mut constraints = Vec::new();
    for e in &collected {
        let n = index
            .neighborhood(&e.id, Some(1), Some(usize::MAX / 4), Some(500), true)?
            .expect("entity exists");
        for b in &n.backlinks {
            if b.rel_type == ACCEPTANCE_CRITERION_RELATION {
                if let Some(t) = &b.target {
                    if t.entity_type == "acceptance_criterion" {
                        acceptance.push((e.id.clone(), t.id.clone(), t.title.clone()));
                    }
                }
            }
        }
        for r in &n.relations {
            if let Some(t) = &r.target {
                if r.rel_type == ACCEPTANCE_CRITERION_RELATION
                    && t.entity_type == "acceptance_criterion"
                {
                    acceptance.push((e.id.clone(), t.id.clone(), t.title.clone()));
                }
                if r.rel_type == CONSTRAINT_RELATION {
                    constraints.push((e.id.clone(), t.id.clone(), t.title.clone()));
                }
            }
        }
        if e.entity_type != "component" {
            continue;
        }
        let groundings = index.groundings_for(&e.id)?;
        if groundings.is_empty() {
            gaps.push(Gap {
                entity_id: e.id.clone(),
                hop: "component → implementation".into(),
                reason: "The component grounds to no code symbol".into(),
            });
            continue;
        }
        for (reference, health, _) in groundings {
            implementations.push((e.id.clone(), reference.clone(), health));
            let Some(gc) = graph.as_ref() else { continue };
            let Ok(RefResolution::Resolved(node)) = resolve_grounding_ref(gc, &reference) else {
                continue;
            };
            let mut stmt = gc.prepare(
                "SELECT n.id, n.file_path FROM edges e JOIN nodes n ON n.id = e.source WHERE e.target = ?1 AND e.kind = 'calls'",
            )?;
            let callers: Vec<(String, String)> = stmt
                .query_map(params![node.id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .filter(|(_, f): &(String, String)| is_test_path(f))
                .collect();
            if callers.is_empty() {
                gaps.push(Gap {
                    entity_id: e.id.clone(),
                    hop: "implementation → test".into(),
                    reason: format!("No test-file symbol calls `{}`", reference),
                });
            }
            for (cid, file) in callers {
                tests.push((reference.clone(), cid, file));
            }
        }
    }
    acceptance.sort();
    acceptance.dedup();
    constraints.sort();
    constraints.dedup();
    gaps.sort_by(|a, b| (&a.entity_id, &a.hop).cmp(&(&b.entity_id, &b.hop)));
    Ok(Some(Traceability {
        origin,
        nodes,
        implementations,
        tests,
        acceptance_criteria: acceptance,
        constraints,
        gaps,
    }))
}
