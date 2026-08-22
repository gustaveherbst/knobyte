//! Write containment for the wiki. Every wiki writer resolves its target through
//! [`check_write_target`] (or writes with [`write_contained`]), so a symlinked directory or
//! file inside the scaffold can never redirect a write outside it.
//!
//! The check is lexical first (no `..`, no absolute paths), then through the real path of the
//! nearest existing ancestor, then on the target itself: a symlinked target file is refused
//! outright, because replacing it would either write elsewhere or silently drop the link.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use crate::wiki::diagnostics::diag;
use crate::wiki::models::WikiDiagnostic;

/// Real path of `absolute` with any missing trailing segments re-appended; `None` when no
/// ancestor exists or cannot be resolved.
pub fn resolve_through_symlinks(absolute: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut cursor = absolute.to_path_buf();
    loop {
        if fs::symlink_metadata(&cursor).is_ok() {
            let mut out = fs::canonicalize(&cursor).ok()?;
            for m in missing.into_iter().rev() {
                out.push(m);
            }
            return Some(out);
        }
        let name = cursor.file_name()?.to_os_string();
        missing.push(name);
        cursor = cursor.parent()?.to_path_buf();
    }
}

fn outside(rel: &str, message: String) -> WikiDiagnostic {
    let mut d = diag("PATH_OUTSIDE_SCAFFOLD", message, rel.to_string());
    d.severity = "error".into();
    d
}

/// Check that writing `rel` (scaffold-relative) stays inside `scaffold_root`: lexically,
/// through every existing parent directory's real path, and without writing through a
/// symlinked file. Returns the absolute path to write on success.
#[allow(clippy::result_large_err)]
pub fn check_write_target(scaffold_root: &Path, rel: &str) -> Result<PathBuf, WikiDiagnostic> {
    let rel_path = Path::new(rel);
    if rel.is_empty()
        || rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(outside(rel, format!("{} is not inside the scaffold root.", rel)));
    }
    let absolute = scaffold_root.join(rel_path);
    let real_root = fs::canonicalize(scaffold_root).unwrap_or_else(|_| scaffold_root.to_path_buf());
    let resolved = resolve_through_symlinks(&absolute)
        .ok_or_else(|| outside(rel, format!("{} could not be resolved inside the scaffold.", rel)))?;
    if !resolved.starts_with(&real_root) || resolved == real_root {
        return Err(outside(
            rel,
            format!(
                "{} resolves to {}, which is outside the scaffold. Nothing was written.",
                rel,
                resolved.display()
            ),
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(&absolute) {
        if meta.file_type().is_symlink() {
            return Err(outside(
                rel,
                format!(
                    "{} is a symlink; the wiki never writes through a symlinked file. Nothing was written.",
                    rel
                ),
            ));
        }
        if !meta.is_file() {
            return Err(outside(
                rel,
                format!("{} is not a regular file. Nothing was written.", rel),
            ));
        }
    }
    Ok(absolute)
}

/// Re-check containment after creating the parent directories (they may have raced in as
/// symlinks), then write through an exclusively created temporary sibling renamed over the
/// target. `rename` replaces a directory entry and never follows a symlink at the target.
#[allow(clippy::result_large_err)]
pub fn write_contained(scaffold_root: &Path, rel: &str, text: &str) -> Result<(), WikiDiagnostic> {
    let target = check_write_target(scaffold_root, rel)?;
    let io = |e: std::io::Error| {
        diag(
            "WRITE_SCOPE_VIOLATION",
            format!("Could not write {}: {}", rel, e),
            rel.to_string(),
        )
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let target = check_write_target(scaffold_root, rel)?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let tmp = target.with_file_name(format!(
        ".{}.{}.kbtmp",
        name,
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &target)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn refuses_escaping_dirs_and_symlinked_files() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let scaffold = root.path().join(".knobyte");
        fs::create_dir_all(scaffold.join("context")).unwrap();
        symlink(out.path(), scaffold.join("context/esc")).unwrap();
        let e = write_contained(&scaffold, "context/esc/new.md", "x").unwrap_err();
        assert_eq!(e.code, "PATH_OUTSIDE_SCAFFOLD");
        assert!(!out.path().join("new.md").exists());

        fs::write(scaffold.join("context/real.md"), "a").unwrap();
        symlink(scaffold.join("context/real.md"), scaffold.join("context/link.md")).unwrap();
        let e = write_contained(&scaffold, "context/link.md", "x").unwrap_err();
        assert_eq!(e.code, "PATH_OUTSIDE_SCAFFOLD");
        assert!(fs::symlink_metadata(scaffold.join("context/link.md"))
            .unwrap()
            .file_type()
            .is_symlink());

        assert!(check_write_target(&scaffold, "../x.md").is_err());
        write_contained(&scaffold, "context/new/deep.md", "ok").unwrap();
        assert_eq!(fs::read_to_string(scaffold.join("context/new/deep.md")).unwrap(), "ok");
    }
}
