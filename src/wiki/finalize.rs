//! Setup's wiki finalization: plan and apply the format migration, capture grounding
//! baselines, rebuild the index, validate, and refuse to call the wiki ready when any step
//! left it unusable.
//!
//! Stages, in order: `plan` (migration inventory + preflight validation), `migration` (apply
//! the audited batch), `grounding` (capture baselines; a reference that cannot be baselined is
//! a skipped entry and fails setup), `index` (full rebuild), `validation` (no error-severity
//! diagnostics), `complete`.

use serde::Serialize;

use crate::config::KnobyteConfig;
use crate::wiki::diagnostics::has_errors;
use crate::wiki::migrate::{migrate, MigrationOptions};
use crate::wiki::models::WikiDiagnostic;
use crate::wiki::scope::WikiScope;
use crate::wiki::validate::{validate_scaffold, ValidateOptions};

/// Diagnostic codes of grounding references that could not be baselined.
pub const SKIPPED_GROUNDING_CODES: [&str; 3] = [
    "GROUNDING_UNRESOLVED",
    "GROUNDING_MISSING",
    "AMBIGUOUS_GROUNDING",
];

/// What [`finalize_wiki_with`] may change.
#[derive(Debug, Clone, Copy)]
pub struct FinalizeOptions {
    /// Apply the format migration to the Markdown (otherwise only plan it and report the
    /// pending changes in `planned_changes`).
    pub apply_migration: bool,
    /// Capture grounding baselines into the Markdown.
    pub capture_baselines: bool,
    /// Fail on validation errors and on groundings that cannot be baselined. When false they
    /// are reported in `warnings` and the wiki is still called ready once its index is built.
    pub strict: bool,
}

impl Default for FinalizeOptions {
    /// The full setup finalization: migrate, capture baselines, strict.
    fn default() -> Self {
        Self { apply_migration: true, capture_baselines: true, strict: true }
    }
}

impl FinalizeOptions {
    /// Leave tracked Markdown untouched: plan the migration, skip baseline capture, rebuild
    /// the local index and report problems as warnings (`knobyte setup` on a populated
    /// scaffold).
    pub fn read_only() -> Self {
        Self { apply_migration: false, capture_baselines: false, strict: false }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WikiFinalization {
    pub ready: bool,
    /// `plan` | `migration` | `grounding` | `index` | `validation` | `complete`
    pub stage: String,
    pub reason: Option<String>,
    pub migrated: bool,
    pub planned_changes: usize,
    pub abstentions: usize,
    pub baselines_captured: usize,
    pub skipped_groundings: Vec<String>,
    pub indexed_entities: usize,
    pub files_scanned: usize,
    pub entities_checked: usize,
    pub warnings: Vec<String>,
    pub diagnostics: Vec<WikiDiagnostic>,
}

impl WikiFinalization {
    fn stop(mut self, stage: &str, reason: String) -> Self {
        self.ready = false;
        self.stage = stage.to_string();
        self.reason = Some(reason);
        self
    }

    /// The setup error message: the stage, the reason and the first blocking diagnostics.
    pub fn failure_message(&self) -> String {
        let mut msg = format!(
            "Wiki finalization failed at {}: {}",
            self.stage,
            self.reason.as_deref().unwrap_or("not ready")
        );
        // Every blocking code, with its count, so nothing hides behind the first eight lines.
        let mut codes: Vec<(String, usize)> = Vec::new();
        for d in self.diagnostics.iter().filter(|d| d.severity == "error") {
            match codes.iter_mut().find(|(c, _)| *c == d.code) {
                Some((_, n)) => *n += 1,
                None => codes.push((d.code.clone(), 1)),
            }
        }
        if !codes.is_empty() {
            let list: Vec<String> = codes
                .iter()
                .map(|(c, n)| if *n > 1 { format!("{} x{}", c, n) } else { c.clone() })
                .collect();
            msg.push_str(&format!(" (diagnostic codes: {})", list.join(", ")));
        }
        let total = codes.iter().map(|(_, n)| n).sum::<usize>();
        let blocking: Vec<String> = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == "error")
            .take(8)
            .map(|d| {
                let loc = match d.line {
                    Some(l) if !d.file.is_empty() => format!(" ({}:{})", d.file, l),
                    _ if !d.file.is_empty() => format!(" ({})", d.file),
                    _ => String::new(),
                };
                format!("\n  [{}] {}{}", d.code, d.message, loc)
            })
            .collect();
        msg.push_str(&blocking.concat());
        if total > blocking.len() {
            msg.push_str(&format!(
                "\n  ... and {} more (run `knobyte wiki validate`)",
                total - blocking.len()
            ));
        }
        for s in self.skipped_groundings.iter().take(8) {
            msg.push_str(&format!("\n  skipped grounding: {}", s));
        }
        msg
    }
}

fn first_errors(diags: &[WikiDiagnostic]) -> Vec<WikiDiagnostic> {
    diags
        .iter()
        .filter(|d| d.severity == "error")
        .cloned()
        .collect()
}

/// Run the full finalization for `config`'s scaffold ([`FinalizeOptions::default`]).
pub fn finalize_wiki(config: &KnobyteConfig) -> WikiFinalization {
    finalize_wiki_with(config, &FinalizeOptions::default())
}

/// Run the finalization for `config`'s scaffold with explicit write permissions.
pub fn finalize_wiki_with(config: &KnobyteConfig, opts: &FinalizeOptions) -> WikiFinalization {
    let scope = WikiScope::load(&config.scaffold_root);
    let graph_db = config.graph_db_path();
    let graph = Some(graph_db.as_path()).filter(|p| p.exists());
    let mut out = WikiFinalization::default();

    // plan + migration
    let result = migrate(
        &MigrationOptions {
            scope: &scope,
            graph_db: graph,
        },
        !opts.apply_migration,
    );
    out.planned_changes = result.plan.items.len();
    out.abstentions = result.plan.abstentions.len();
    out.warnings
        .extend(crate::wiki::migrate::abstention_messages(&result.plan));
    if result.plan.blocked && opts.strict {
        out.diagnostics = first_errors(&result.plan.diagnostics);
        return out.stop(
            "plan",
            "the wiki migration is blocked by errors in the populated scaffold".into(),
        );
    }
    if let Some(report) = &result.report {
        if !report.ok {
            out.diagnostics = first_errors(&report.diagnostics);
            return out.stop(
                "migration",
                "the wiki migration could not be applied safely".into(),
            );
        }
        out.migrated = !report.changed_files.is_empty();
    }

    // grounding baselines
    if let (Some(db), true) = (graph, opts.capture_baselines) {
        match crate::graph::GraphEngine::open(db)
            .map_err(|e| e.to_string())
            .and_then(|engine| {
                engine
                    .ground_docs(&config.project_root, &config.scaffold_root)
                    .map_err(|e| e.to_string())
            }) {
            Ok(n) => out.baselines_captured = n,
            Err(e) => return out.stop("grounding", format!("grounding capture failed: {}", e)),
        }
    }

    // index
    let rebuilt = crate::wiki::WikiIndex::open_for_rebuild(&config.wiki_db_path())
        .and_then(|mut i| i.rebuild(&config.scaffold_root));
    match rebuilt {
        Ok(n) => out.indexed_entities = n,
        Err(e) => return out.stop("index", format!("the wiki index rebuild failed: {}", e)),
    }

    // validation
    let index = crate::wiki::WikiIndex::open_read_only(&config.wiki_db_path()).ok();
    let report = validate_scaffold(&ValidateOptions {
        scope: &scope,
        project_root: &config.project_root,
        graph_db: graph,
        index: index.as_ref(),
        limit: None,
    });
    out.files_scanned = report.files_scanned;
    out.entities_checked = report.entities_checked;
    for d in report
        .diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
    {
        out.warnings.push(format!(
            "{}{}: {}",
            d.code,
            if d.file.is_empty() {
                String::new()
            } else {
                format!(" {}", d.file)
            },
            d.message
        ));
    }
    if graph.is_some() {
        out.skipped_groundings = report
            .diagnostics
            .iter()
            .filter(|d| SKIPPED_GROUNDING_CODES.contains(&d.code.as_str()))
            .map(|d| format!("{} ({})", d.message, d.file))
            .collect();
    }
    if !opts.strict {
        for d in report.diagnostics.iter().filter(|d| d.severity == "error") {
            out.warnings.push(format!(
                "{}{}: {}",
                d.code,
                if d.file.is_empty() { String::new() } else { format!(" {}", d.file) },
                d.message
            ));
        }
        out.diagnostics = first_errors(&report.diagnostics);
        out.ready = true;
        out.stage = "complete".into();
        return out;
    }
    if has_errors(&report.diagnostics) {
        out.diagnostics = first_errors(&report.diagnostics);
        return out.stop("validation", "wiki validation found blocking errors".into());
    }
    if !out.skipped_groundings.is_empty() {
        let n = out.skipped_groundings.len();
        return out.stop(
            "grounding",
            format!(
                "{} grounding reference(s) could not be baselined; fix them (or run `knobyte sync`) and rerun setup",
                n
            ),
        );
    }
    out.ready = true;
    out.stage = "complete".into();
    out
}
