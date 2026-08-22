//! Generated views: sections between `<!-- kb:generated:begin type=<entity type> -->` and
//! `<!-- kb:generated:end -->` are rendered from the wiki and rewritten by
//! `knobyte wiki regenerate-views`. Nothing outside the markers is read for the view or
//! written. Without a `type=` attribute the type is inferred from the path
//! (`patterns/INDEX.md` / `patterns/README.md` → pattern, `decisions.md` → decision).

use serde::{Deserialize, Serialize};

use crate::wiki::diagnostics::diag;
use crate::wiki::markdown::{dominant_eol, fence_closes, fence_open, split_lines};
use crate::wiki::models::{WikiDiagnostic, WikiEntity};
use crate::wiki::parser::ParsedFile;
use crate::wiki::scope::WikiScope;
use crate::wiki::validate::parse_corpus;

pub const GENERATED_BEGIN: &str = "<!-- kb:generated:begin";
pub const GENERATED_END: &str = "<!-- kb:generated:end -->";

#[derive(Debug, Clone)]
pub struct GeneratedRegion {
    pub start: usize,
    pub end: usize,
    /// The begin marker line as written (without terminator).
    pub begin_line: String,
    pub entity_type: Option<String>,
}

fn infer_view_type(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if (lower.starts_with("patterns/") || lower.contains("/patterns/"))
        && (name == "index.md" || name == "readme.md")
    {
        return Some("pattern".into());
    }
    if name == "decisions.md" || name == "decision.md" {
        return Some("decision".into());
    }
    None
}

/// Generated regions of a file (code-fence safe).
pub fn find_regions(path: &str, text: &str) -> Vec<GeneratedRegion> {
    let lines = split_lines(text);
    let mut out = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut open: Option<(usize, String, Option<String>)> = None;
    for l in &lines {
        let line = &text[l.start..l.content_end];
        if let Some((ch, n)) = fence {
            if fence_closes(line, ch, n) {
                fence = None;
            }
            continue;
        }
        if open.is_none() {
            if let Some(f) = fence_open(line) {
                fence = Some(f);
                continue;
            }
        }
        let t = line.trim();
        if open.is_none() && t.starts_with(GENERATED_BEGIN) && t.ends_with("-->") {
            let attrs = t[GENERATED_BEGIN.len()..t.len() - 3].trim();
            let ty = attrs
                .split_whitespace()
                .find_map(|a| a.strip_prefix("type="))
                .map(|s| s.trim_matches(['"', '\'']).to_string())
                .or_else(|| infer_view_type(path));
            open = Some((l.start, t.to_string(), ty));
        } else if t == GENERATED_END {
            if let Some((start, begin_line, ty)) = open.take() {
                out.push(GeneratedRegion {
                    start,
                    end: l.start + line.find(GENERATED_END).unwrap_or(0) + GENERATED_END.len(),
                    begin_line,
                    entity_type: ty,
                });
            }
        }
    }
    out
}

/// Render the view body for `entity_type`.
pub fn render_view(begin_line: &str, rows: &[&WikiEntity], eol: &str) -> String {
    let mut lines: Vec<String> = vec![
        begin_line.to_string(),
        String::new(),
        "| Entity | Status | Where |".into(),
        "|---|---|---|".into(),
    ];
    for e in rows {
        lines.push(format!(
            "| {} | {} | `{}` |",
            e.title.replace('|', "\\|"),
            e.status,
            e.file
        ));
    }
    if rows.is_empty() {
        lines.push("| _none yet_ | | |".into());
    }
    lines.push(String::new());
    lines.push(GENERATED_END.to_string());
    lines.join(eol)
}

fn rows_for<'a>(files: &'a [ParsedFile], entity_type: &str) -> Vec<&'a WikiEntity> {
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<&WikiEntity> = files
        .iter()
        .flat_map(|f| f.entities.iter().map(|e| &e.entity))
        .filter(|e| e.entity_type == entity_type && seen.insert(e.id.clone()))
        .collect();
    rows.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// Re-render every generated region of `file`; returns the new text (None if unchanged) and
/// diagnostics for regions whose type cannot be determined.
fn render_file(
    file: &ParsedFile,
    corpus: &[ParsedFile],
) -> (Option<String>, usize, Vec<WikiDiagnostic>) {
    let regions = find_regions(&file.path, &file.text);
    let eol = dominant_eol(&file.text);
    let mut out = String::new();
    let mut cursor = 0;
    let mut diags = Vec::new();
    let mut stale = 0;
    for r in &regions {
        let Some(ty) = &r.entity_type else {
            diags.push(diag(
                "GENERATED_VIEW_DRIFT",
                format!(
                    "The generated section in {} has no `type=` attribute and none can be inferred from its path",
                    file.path
                ),
                file.path.clone(),
            ));
            continue;
        };
        let rendered = render_view(&r.begin_line, &rows_for(corpus, ty), eol);
        if file.text[r.start..r.end] != rendered {
            stale += 1;
        }
        out.push_str(&file.text[cursor..r.start]);
        out.push_str(&rendered);
        cursor = r.end;
    }
    out.push_str(&file.text[cursor..]);
    (if stale > 0 { Some(out) } else { None }, stale, diags)
}

/// `GENERATED_VIEW_DRIFT` for every stale generated section.
pub fn view_drift_diagnostics(_scope: &WikiScope, files: &[ParsedFile]) -> Vec<WikiDiagnostic> {
    let mut out = Vec::new();
    for f in files {
        if !f.text.contains(GENERATED_BEGIN) {
            continue;
        }
        let (changed, _, diags) = render_file(f, files);
        out.extend(diags);
        if changed.is_some() {
            out.push(diag(
                "GENERATED_VIEW_DRIFT",
                format!(
                    "The generated section in {} no longer matches the wiki. Nothing outside the markers was read or will be written",
                    f.path
                ),
                f.path.clone(),
            ));
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegeneratedFile {
    pub file: String,
    pub regions: usize,
    pub stale_regions: usize,
    pub written: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegenerateReport {
    pub dry_run: bool,
    pub files: Vec<RegeneratedFile>,
    pub diagnostics: Vec<WikiDiagnostic>,
}

/// Rewrite stale generated sections (only the text between the markers changes).
pub fn regenerate_views(scope: &WikiScope, dry_run: bool) -> RegenerateReport {
    let (files, _) = parse_corpus(scope);
    let mut report = RegenerateReport {
        dry_run,
        files: Vec::new(),
        diagnostics: Vec::new(),
    };
    for f in &files {
        if !f.text.contains(GENERATED_BEGIN) {
            continue;
        }
        let regions = find_regions(&f.path, &f.text).len();
        if regions == 0 {
            continue;
        }
        let (changed, stale, diags) = render_file(f, &files);
        report.diagnostics.extend(diags);
        let mut written = false;
        if let Some(new_text) = changed {
            if scope.is_read_only(&f.path) {
                report.diagnostics.push(diag(
                    "WRITE_SCOPE_VIOLATION",
                    format!(
                        "{} is read-only to the wiki; its generated section was not rewritten",
                        f.path
                    ),
                    f.path.clone(),
                ));
            } else if !dry_run {
                match crate::wiki::paths::write_contained(&scope.scaffold_root, &f.path, &new_text) {
                    Ok(()) => written = true,
                    Err(d) => report.diagnostics.push(d),
                }
            }
        }
        report.files.push(RegeneratedFile {
            file: f.path.clone(),
            regions,
            stale_regions: stale,
            written,
        });
    }
    report
}
