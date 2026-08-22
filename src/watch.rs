//! `knobyte watch`: install/uninstall a post-commit hook that runs the drift check, or run the
//! heartbeat on an interval.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const HOOK_BEGIN: &str = "# knobyte-drift-check";
pub const HOOK_END: &str = "# knobyte-drift-check:end";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Installed(PathBuf),
    Appended(PathBuf),
    AlreadyInstalled(PathBuf),
    Removed(PathBuf),
    SectionRemoved(PathBuf),
    NotInstalled(PathBuf),
    ForeignHook(PathBuf),
}

impl HookOutcome {
    pub fn message(&self) -> String {
        match self {
            HookOutcome::Installed(p) => format!("Installed the Knobyte post-commit hook at {}.", p.display()),
            HookOutcome::Appended(p) => format!("Added the Knobyte drift check to the existing post-commit hook at {}.", p.display()),
            HookOutcome::AlreadyInstalled(p) => format!("The Knobyte post-commit hook is already installed at {}.", p.display()),
            HookOutcome::Removed(p) => format!("Removed the Knobyte post-commit hook ({}).", p.display()),
            HookOutcome::SectionRemoved(p) => format!("Removed the Knobyte section from the post-commit hook at {}.", p.display()),
            HookOutcome::NotInstalled(p) => format!("No post-commit hook found at {}.", p.display()),
            HookOutcome::ForeignHook(p) => format!("The post-commit hook at {} was not installed by Knobyte; left unchanged.", p.display()),
        }
    }
}

/// The hooks directory git uses for this checkout (honours worktrees and `core.hooksPath`).
pub fn hooks_dir(project_root: &Path) -> Result<PathBuf, String> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(project_root)
        .output()
        .map_err(|e| format!("git is not available: {}", e))?;
    if !out.status.success() {
        return Err("Not a git repository; `knobyte watch` needs one (run `git init`).".into());
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let p = PathBuf::from(&rel);
    Ok(if p.is_absolute() { p } else { project_root.join(p) })
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The hook section (between [`HOOK_BEGIN`] and [`HOOK_END`]).
pub fn hook_section(knobyte_exe: &str) -> String {
    format!(
        "{begin}\n# Installed by `knobyte watch`: runs the drift check after each commit.\nif command -v knobyte >/dev/null 2>&1; then KNOBYTE=knobyte; else KNOBYTE={exe}; fi\nSCORE=$(\"$KNOBYTE\" check --quiet 2>&1) || true\ncase \"$SCORE\" in\n  *\"100/100\"*) ;;\n  *) echo \"$SCORE\" ;;\nesac\n{end}\n",
        begin = HOOK_BEGIN,
        exe = quote(knobyte_exe),
        end = HOOK_END
    )
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}
#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), String> {
    Ok(())
}

pub fn install_hook(project_root: &Path, knobyte_exe: &str) -> Result<HookOutcome, String> {
    let dir = hooks_dir(project_root)?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let hook = dir.join("post-commit");
    let section = hook_section(knobyte_exe);
    match fs::read_to_string(&hook) {
        Ok(existing) if existing.contains(HOOK_BEGIN) => Ok(HookOutcome::AlreadyInstalled(hook)),
        Ok(existing) => {
            let updated = format!("{}\n\n{}", existing.trim_end(), section);
            fs::write(&hook, updated).map_err(|e| e.to_string())?;
            make_executable(&hook)?;
            Ok(HookOutcome::Appended(hook))
        }
        Err(_) => {
            fs::write(&hook, format!("#!/bin/sh\n{}", section)).map_err(|e| e.to_string())?;
            make_executable(&hook)?;
            Ok(HookOutcome::Installed(hook))
        }
    }
}

pub fn uninstall_hook(project_root: &Path) -> Result<HookOutcome, String> {
    let hook = hooks_dir(project_root)?.join("post-commit");
    let Ok(content) = fs::read_to_string(&hook) else {
        return Ok(HookOutcome::NotInstalled(hook));
    };
    let Some(start) = content.find(HOOK_BEGIN) else {
        return Ok(HookOutcome::ForeignHook(hook));
    };
    let end = content[start..]
        .find(HOOK_END)
        .map(|i| start + i + HOOK_END.len())
        .unwrap_or(content.len());
    let remaining = format!("{}{}", &content[..start], &content[end..]);
    let trimmed = remaining.trim();
    if trimmed.is_empty() || trimmed == "#!/bin/sh" {
        fs::remove_file(&hook).map_err(|e| e.to_string())?;
        Ok(HookOutcome::Removed(hook))
    } else {
        fs::write(&hook, format!("{}\n", trimmed)).map_err(|e| e.to_string())?;
        make_executable(&hook)?;
        Ok(HookOutcome::SectionRemoved(hook))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_append_uninstall() {
        let d = tempfile::tempdir().unwrap();
        let ok = Command::new("git").args(["init", "-q"]).current_dir(d.path()).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return;
        }
        let hooks = hooks_dir(d.path()).unwrap();
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("post-commit"), "#!/bin/sh\necho mine\n").unwrap();
        assert!(matches!(install_hook(d.path(), "/bin/knobyte").unwrap(), HookOutcome::Appended(_)));
        assert!(matches!(install_hook(d.path(), "/bin/knobyte").unwrap(), HookOutcome::AlreadyInstalled(_)));
        assert!(matches!(uninstall_hook(d.path()).unwrap(), HookOutcome::SectionRemoved(_)));
        assert_eq!(fs::read_to_string(hooks.join("post-commit")).unwrap(), "#!/bin/sh\necho mine\n");
        fs::remove_file(hooks.join("post-commit")).unwrap();
        assert!(matches!(install_hook(d.path(), "/bin/knobyte").unwrap(), HookOutcome::Installed(_)));
        assert!(matches!(uninstall_hook(d.path()).unwrap(), HookOutcome::Removed(_)));
        assert!(!hooks.join("post-commit").exists());
    }
}
