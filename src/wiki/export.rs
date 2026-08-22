//! `knobyte export`: the whole scaffold as one Markdown bundle.
//!
//! The bundle starts with a fixed marker line so a later export may overwrite a previous bundle
//! but never any other file. Size limits keep the bundle reviewable.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::graph::grounding::scaffold_markdown_files;

pub const MAX_EXPORT_FILES: usize = 1000;
pub const MAX_EXPORT_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_EXPORT_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
pub const BUNDLE_MARKER: &str = "# knobyte scaffold export\n";

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub document: String,
    pub files: Vec<String>,
    /// Where the bundle was written (None when returned for stdout).
    pub written_to: Option<PathBuf>,
}

fn canonical_best_effort(p: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut current = p.to_path_buf();
    while !current.exists() {
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
    let mut out = fs::canonicalize(&current).unwrap_or(current);
    for m in missing.into_iter().rev() {
        out.push(m);
    }
    out
}

/// Read at most `cap` bytes of `p`; `None` when the file holds more than `cap` bytes.
fn read_capped(p: &Path, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    fs::File::open(p)?.take(cap + 1).read_to_end(&mut buf)?;
    Ok(if buf.len() as u64 > cap { None } else { Some(buf) })
}

fn is_previous_bundle(p: &Path) -> bool {
    // A symlink is never treated as a previous bundle: replacing it would write elsewhere.
    if fs::symlink_metadata(p).map(|m| !m.is_file()).unwrap_or(true) {
        return false;
    }
    let mut head = Vec::new();
    match fs::File::open(p) {
        Ok(f) => {
            if f.take(BUNDLE_MARKER.len() as u64)
                .read_to_end(&mut head)
                .is_err()
            {
                return false;
            }
            head == BUNDLE_MARKER.as_bytes()
        }
        Err(_) => false,
    }
}

/// Write the bundle without ever following or clobbering a file that appeared after the
/// checks: a new target is created exclusively (O_EXCL); a previous bundle is replaced by
/// an exclusively created sibling renamed over it.
fn write_bundle(target: &Path, doc: &str, replace_previous: bool) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("Could not write {}: {}", target.display(), e);
    if !replace_previous {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    format!(
                        "Refusing to export: \"{}\" appeared while exporting. Choose a different --out path.",
                        target.display()
                    )
                } else {
                    fail(e)
                }
            })?;
        return f.write_all(doc.as_bytes()).map_err(fail);
    }
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "bundle".into());
    let tmp = target.with_file_name(format!(".{}.{}.tmp", name, uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(doc.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, target)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(fail(e));
    }
    Ok(())
}

/// Build (and, with `out`, write) the export bundle. `out` is resolved against
/// `project_root`.
pub fn export_scaffold(
    project_root: &Path,
    scaffold_root: &Path,
    out: Option<&str>,
) -> Result<ExportResult, String> {
    let mut files = scaffold_markdown_files(scaffold_root);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let target = out.map(|o| {
        let p = Path::new(o);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            project_root.join(p)
        }
    });
    if let Some(t) = &target {
        let t_real = canonical_best_effort(t);
        let config = canonical_best_effort(&scaffold_root.join("config.json"));
        if t_real == config {
            return Err(format!(
                "Refusing to export: \"{}\" would overwrite the project configuration",
                out.unwrap_or_default()
            ));
        }
        let previous = is_previous_bundle(t);
        let exists = fs::symlink_metadata(t).is_ok();
        if exists && !previous {
            return Err(format!(
                "Refusing to export: \"{}\" already exists and is not a previous export bundle. Choose a different --out path.",
                out.unwrap_or_default()
            ));
        }
        // A previous bundle inside the scaffold is not exported into itself.
        files.retain(|(_, abs)| canonical_best_effort(abs) != t_real);
    }
    if files.is_empty() {
        return Err("No scaffold files found. Run: knobyte setup".to_string());
    }
    if files.len() > MAX_EXPORT_FILES {
        return Err(format!(
            "Scaffold has {} files; export supports at most {}.",
            files.len(),
            MAX_EXPORT_FILES
        ));
    }
    let mut total: u64 = 0;
    let mut doc = String::from(BUNDLE_MARKER);
    doc.push('\n');
    let mut names = Vec::new();
    for (rel, abs) in &files {
        let meta = fs::metadata(abs).map_err(|e| format!("Could not read metadata of {}: {}", abs.display(), e))?;
        if !meta.is_file() {
            return Err(format!(
                "Refusing to export: \"{}\" is not a regular file.",
                rel
            ));
        }
        if meta.len() > MAX_EXPORT_FILE_BYTES {
            return Err(format!(
                "{} is larger than {} bytes; export supports at most {} bytes per file.",
                rel, MAX_EXPORT_FILE_BYTES, MAX_EXPORT_FILE_BYTES
            ));
        }
        total += meta.len();
        if total > MAX_EXPORT_TOTAL_BYTES {
            return Err(format!(
                "Scaffold totals more than {} bytes; export supports at most {} bytes in total.",
                MAX_EXPORT_TOTAL_BYTES, MAX_EXPORT_TOTAL_BYTES
            ));
        }
        // Capped read: the file may have grown since it was measured.
        let bytes = read_capped(abs, MAX_EXPORT_FILE_BYTES)
            .map_err(|e| format!("Could not read {}: {}", abs.display(), e))?
            .ok_or_else(|| {
                format!(
                    "{} is larger than {} bytes; export supports at most {} bytes per file.",
                    rel, MAX_EXPORT_FILE_BYTES, MAX_EXPORT_FILE_BYTES
                )
            })?;
        let content = String::from_utf8_lossy(&bytes);
        doc.push_str(&format!("## {}\n\n{}\n\n", rel, content.trim_end()));
        names.push(rel.clone());
    }
    let doc = format!("{}\n", doc.trim_end());
    if let Some(t) = &target {
        if let Some(parent) = t.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                format!("Could not create directory {}: {}", parent.display(), e)
            })?;
        }
        write_bundle(t, &doc, is_previous_bundle(t))?;
    }
    Ok(ExportResult {
        document: doc,
        files: names,
        written_to: target,
    })
}
