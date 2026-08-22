use std::collections::BTreeMap;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::KnobyteConfig;
use crate::graph::grounding::{resolve_grounding_ref, CommittedIndex, RefResolution};
use crate::team::envelope::{paginate, Page, TeamError};
use crate::team::inbox::{lifecycle_of, LocatedEntity};
use crate::wiki::index::{committed_index, doc_ref_of, grounding_health, health_rank};
use crate::wiki::parser::parse_markdown_entity;

pub const LIFECYCLE_STATES: &[&str] = &["in_flight", "promoted", "deprecated", "archived"];
pub const GROUNDING_HEALTH: &[&str] = &["fresh", "changed", "missing", "ambiguous", "unverified"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecItem {
    pub id: String,
    pub title: String,
    pub summary: Option<String>,
    pub status: String,
    pub file: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, rename = "lifecycleState")]
    pub lifecycle_state: String,
    #[serde(default, rename = "groundingHealth")]
    pub grounding_health: String,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroundingStatus {
    pub reference: String,
    /// fresh, changed, missing, ambiguous or unverified.
    pub health: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HierarchyRelation {
    #[serde(rename = "type")]
    pub rel_type: String,
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpecHierarchy {
    pub requirements: Vec<SpecItem>,
    #[serde(rename = "acceptanceCriteria")]
    pub acceptance_criteria: Vec<SpecItem>,
    pub constraints: Vec<SpecItem>,
    pub relations: Vec<HierarchyRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecDetail {
    #[serde(flatten)]
    pub item: SpecItem,
    /// Markdown body (without frontmatter).
    pub body: String,
    /// Full raw file content, including frontmatter.
    pub content: String,
    #[serde(default)]
    pub groundings: Vec<GroundingStatus>,
    #[serde(default)]
    pub hierarchy: SpecHierarchy,
    /// Count of grounding health over the spec and its hierarchy.
    #[serde(default, rename = "groundingRollup")]
    pub grounding_rollup: BTreeMap<String, usize>,
}

struct Corpus {
    entities: Vec<LocatedEntity>,
    graph: Option<Connection>,
    /// Committed grounding baselines of every document (drift's scaffold-wide index).
    committed: CommittedIndex,
}

impl Corpus {
    fn load(config: &KnobyteConfig) -> Corpus {
        let mut entities = Vec::new();
        let local = config.local_dir();
        for entry in WalkDir::new(&config.scaffold_root).into_iter().filter_entry(|e| e.path() != local).filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let rel = crate::team::workflow::rel_path(config, path);
            if rel.split('/').any(|p| p.starts_with('.')) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Some(entity) = parse_markdown_entity(&rel, &content) {
                    entities.push(LocatedEntity { rel, entity, content });
                }
            }
        }
        entities.sort_by(|a, b| a.rel.cmp(&b.rel));
        let graph_path = config.graph_db_path();
        let graph = if graph_path.exists() {
            Connection::open_with_flags(&graph_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
        } else {
            None
        };
        let committed = committed_index(entities.iter().map(|e| &e.entity));
        Corpus { entities, graph, committed }
    }

    fn groundings(&self, e: &LocatedEntity) -> Vec<GroundingStatus> {
        e.entity
            .grounds_to
            .iter()
            .map(|r| {
                // Same health source as the wiki index (baseline-aware: fresh,
                // changed, missing, ambiguous, unverified), so both agree.
                let (health, _) = grounding_health(
                    self.graph.as_ref(),
                    &e.entity.file,
                    &doc_ref_of(&e.entity, r),
                    &self.committed,
                );
                let resolved = match (&self.graph, health.as_str()) {
                    (Some(conn), "fresh" | "changed" | "unverified") => match resolve_grounding_ref(conn, r) {
                        Ok(RefResolution::Resolved(n)) => Some(n.id.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                GroundingStatus { reference: r.clone(), health, resolved }
            })
            .collect()
    }

    /// Worst grounding health of a document (wiki ranking); `unverified` without groundings.
    fn health(&self, e: &LocatedEntity) -> String {
        self.groundings(e)
            .into_iter()
            .map(|g| g.health)
            .max_by_key(|h| health_rank(Some(h)))
            .unwrap_or_else(|| "unverified".to_string())
    }

    fn item(&self, e: &LocatedEntity) -> SpecItem {
        SpecItem {
            id: e.entity.id.clone(),
            title: e.entity.title.clone(),
            summary: e.entity.summary.clone(),
            status: e.entity.status.clone(),
            file: e.entity.file.clone(),
            kind: e.entity.entity_type.clone(),
            lifecycle_state: lifecycle_of(&e.entity.status),
            grounding_health: self.health(e),
            topics: e.entity.topics.clone(),
            revision: e.entity.revision,
        }
    }

    fn by_id(&self, id: &str) -> Option<&LocatedEntity> {
        self.entities.iter().find(|e| e.entity.id == id)
    }
}

fn is_spec_file(config: &KnobyteConfig, e: &LocatedEntity) -> bool {
    let specs_rel = crate::team::workflow::rel_path(config, &config.specs_dir());
    e.rel.starts_with(&format!("{}/", specs_rel))
}

/// All Markdown documents under `specs/` (any kind), sorted by title.
pub fn list_specs(config: &KnobyteConfig) -> Vec<SpecItem> {
    let corpus = Corpus::load(config);
    let mut specs: Vec<SpecItem> = corpus.entities.iter().filter(|e| is_spec_file(config, e)).map(|e| corpus.item(e)).collect();
    specs.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
    specs
}

#[derive(Debug, Clone, Default)]
pub struct SpecListFilter {
    pub lifecycle: Option<String>,
    pub grounding: Option<String>,
    pub topic: Option<String>,
    pub include_archived: bool,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

/// Paged list of top-level specs (`kind == spec`), filtered by lifecycle,
/// grounding health and topic. Archived specs are hidden unless requested.
pub fn list_specs_page(config: &KnobyteConfig, f: &SpecListFilter) -> Result<Page<SpecItem>, TeamError> {
    if let Some(l) = &f.lifecycle {
        if !LIFECYCLE_STATES.contains(&l.as_str()) {
            return Err(TeamError::usage(format!("--lifecycle must be one of: {}", LIFECYCLE_STATES.join(", "))));
        }
    }
    if let Some(g) = &f.grounding {
        if !GROUNDING_HEALTH.contains(&g.as_str()) {
            return Err(TeamError::usage(format!("--grounding must be one of: {}", GROUNDING_HEALTH.join(", "))));
        }
    }
    let corpus = Corpus::load(config);
    let mut items: Vec<SpecItem> = corpus
        .entities
        .iter()
        .filter(|e| e.entity.entity_type == "spec")
        .map(|e| corpus.item(e))
        .filter(|s| match &f.lifecycle {
            Some(l) => &s.lifecycle_state == l,
            None => f.include_archived || s.lifecycle_state != "archived",
        })
        .filter(|s| f.grounding.as_ref().map(|g| &s.grounding_health == g).unwrap_or(true))
        .filter(|s| f.topic.as_ref().map(|t| s.topics.iter().any(|x| x == t)).unwrap_or(true))
        .collect();
    items.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
    paginate(
        items,
        |s| format!("{}@{}", s.id, s.revision),
        &format!("{:?}|{:?}|{:?}|{}", f.lifecycle, f.grounding, f.topic, f.include_archived),
        f.cursor.as_deref(),
        f.limit,
    )
}

fn relates(e: &LocatedEntity, rel_type: &str, targets: &[String]) -> Option<String> {
    e.entity
        .relations
        .iter()
        .find(|r| r.rel_type == rel_type && targets.contains(&r.target_id))
        .map(|r| r.target_id.clone())
}

fn hierarchy(corpus: &Corpus, root: &LocatedEntity) -> SpecHierarchy {
    let mut h = SpecHierarchy::default();
    let root_id = root.entity.id.clone();
    // Requirements derived from the spec, then requirements refining those (transitively).
    let mut req_ids: Vec<String> = Vec::new();
    loop {
        let mut added = false;
        for e in corpus.entities.iter().filter(|e| e.entity.entity_type == "requirement") {
            if req_ids.contains(&e.entity.id) {
                continue;
            }
            let link = relates(e, "derived_from", std::slice::from_ref(&root_id))
                .map(|t| ("derived_from", t))
                .or_else(|| relates(e, "refines", &req_ids).map(|t| ("refines", t)));
            if let Some((rel, target)) = link {
                req_ids.push(e.entity.id.clone());
                h.relations.push(HierarchyRelation { rel_type: rel.to_string(), source: e.entity.id.clone(), target });
                h.requirements.push(corpus.item(e));
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    let mut verified_targets = req_ids.clone();
    verified_targets.push(root_id.clone());
    for e in corpus.entities.iter().filter(|e| e.entity.entity_type == "acceptance_criterion") {
        if let Some(t) = relates(e, "verified_by", &verified_targets) {
            h.relations.push(HierarchyRelation { rel_type: "verified_by".to_string(), source: e.entity.id.clone(), target: t });
            h.acceptance_criteria.push(corpus.item(e));
        }
    }
    // Constraints referenced (constrained_by) by the spec or any member of its hierarchy.
    let mut members: Vec<&LocatedEntity> = vec![root];
    members.extend(req_ids.iter().filter_map(|id| corpus.by_id(id)));
    members.extend(h.acceptance_criteria.iter().filter_map(|a| corpus.by_id(&a.id)));
    let mut constraint_ids: Vec<String> = Vec::new();
    for m in members {
        for r in m.entity.relations.iter().filter(|r| r.rel_type == "constrained_by") {
            if let Some(c) = corpus.by_id(&r.target_id).filter(|c| c.entity.entity_type == "constraint") {
                h.relations.push(HierarchyRelation { rel_type: "constrained_by".to_string(), source: m.entity.id.clone(), target: c.entity.id.clone() });
                if !constraint_ids.contains(&c.entity.id) {
                    constraint_ids.push(c.entity.id.clone());
                    h.constraints.push(corpus.item(c));
                }
            }
        }
    }
    h
}

/// Look up a single spec by its frontmatter id, or by its scaffold-relative file
/// path (e.g. `specs/auth.md`), with its hierarchy and grounding rollup.
pub fn get_spec(config: &KnobyteConfig, id: &str) -> Result<SpecDetail, String> {
    let wanted = id.trim();
    let wanted_path = wanted.trim_start_matches(".knobyte/");
    let corpus = Corpus::load(config);
    let found = corpus
        .entities
        .iter()
        .find(|e| (is_spec_file(config, e) || crate::team::inbox::SPEC_KINDS.contains(&e.entity.entity_type.as_str())) && (e.entity.id == wanted || e.rel == wanted_path))
        .ok_or_else(|| format!("Spec '{}' not found", id))?;
    let h = hierarchy(&corpus, found);
    let mut rollup: BTreeMap<String, usize> = BTreeMap::new();
    let mut all = vec![found];
    for list in [&h.requirements, &h.acceptance_criteria, &h.constraints] {
        all.extend(list.iter().filter_map(|i| corpus.by_id(&i.id)));
    }
    for e in all {
        for g in corpus.groundings(e) {
            *rollup.entry(g.health).or_insert(0) += 1;
        }
    }
    Ok(SpecDetail {
        item: corpus.item(found),
        body: found.entity.body.clone(),
        content: found.content.clone(),
        groundings: corpus.groundings(found),
        hierarchy: h,
        grounding_rollup: rollup,
    })
}
