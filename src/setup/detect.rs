//! Detecting which AI coding tools a developer uses, so `knobyte setup` can preselect them.
//!
//! Signals, per tool (any one is enough):
//! - a CLI on PATH (`claude`, `codex`, `cursor`, `windsurf`, `code` / `code-insiders` for
//!   Copilot, `opencode`);
//! - a macOS app bundle (`Cursor.app`, `Windsurf.app`, `Visual Studio Code.app`);
//! - a user configuration directory (`~/.claude`, `~/.codex`, `~/.cursor`,
//!   `~/.codeium/windsurf`, `~/.config/opencode`);
//! - a project directory (`.claude`, `.codex`, `.cursor`, `.vscode`, `.windsurf`, `.opencode`).
//!
//! Detection only reads the file system; it never runs a tool.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::agent::find_on_path;
use crate::config::AI_TOOLS;

/// Overrides the directory searched for macOS app bundles (`/Applications`); tests point it at
/// an empty directory so the machine's installed apps do not leak in.
pub const APPLICATIONS_DIR_ENV: &str = "KNOBYTE_APPLICATIONS_DIR";

/// Where detection looks.
#[derive(Debug, Clone)]
pub struct DetectEnv {
    /// PATH to search (`None`: no CLI lookup).
    pub path: Option<OsString>,
    /// The user's home directory (`None`: no user config lookup).
    pub home: Option<PathBuf>,
    /// Directory holding app bundles (`None`: no app lookup).
    pub applications: Option<PathBuf>,
    pub project_root: PathBuf,
}

/// The current user's home directory (`HOME`, or `USERPROFILE` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}

impl DetectEnv {
    /// Detection environment of this process.
    pub fn from_process(project_root: &Path) -> Self {
        let applications = match std::env::var_os(APPLICATIONS_DIR_ENV) {
            Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
            Some(_) => None,
            None if cfg!(target_os = "macos") => Some(PathBuf::from("/Applications")),
            None => None,
        };
        DetectEnv { path: std::env::var_os("PATH"), home: home_dir(), applications, project_root: project_root.to_path_buf() }
    }
}

/// One detected tool and the evidence for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetectedTool {
    pub tool: String,
    /// Human-readable evidence, e.g. `claude on PATH`, `~/.cursor`.
    pub signals: Vec<String>,
}

struct Signals {
    tool: &'static str,
    clis: &'static [&'static str],
    apps: &'static [&'static str],
    user_dirs: &'static [&'static str],
    project_dirs: &'static [&'static str],
}

const SIGNALS: &[Signals] = &[
    Signals { tool: "claude", clis: &["claude"], apps: &[], user_dirs: &[".claude"], project_dirs: &[".claude"] },
    Signals {
        tool: "cursor",
        clis: &["cursor"],
        apps: &["Cursor.app"],
        user_dirs: &[".cursor"],
        project_dirs: &[".cursor"],
    },
    Signals {
        tool: "windsurf",
        clis: &["windsurf"],
        apps: &["Windsurf.app"],
        user_dirs: &[".codeium/windsurf"],
        project_dirs: &[".windsurf"],
    },
    Signals {
        tool: "copilot",
        clis: &["code", "code-insiders"],
        apps: &["Visual Studio Code.app"],
        user_dirs: &[],
        project_dirs: &[".vscode"],
    },
    Signals {
        tool: "opencode",
        clis: &["opencode"],
        apps: &[],
        user_dirs: &[".config/opencode"],
        project_dirs: &[".opencode"],
    },
    Signals { tool: "codex", clis: &["codex"], apps: &[], user_dirs: &[".codex"], project_dirs: &[".codex"] },
];

/// Detected tools, in [`AI_TOOLS`] order.
pub fn detect_tools(env: &DetectEnv) -> Vec<DetectedTool> {
    let mut out = Vec::new();
    for tool in AI_TOOLS {
        let Some(s) = SIGNALS.iter().find(|s| s.tool == *tool) else { continue };
        let mut signals = Vec::new();
        if let Some(path) = env.path.as_deref() {
            for cli in s.clis {
                if find_on_path(cli, Some(path)).is_some() {
                    signals.push(format!("{} on PATH", cli));
                }
            }
        }
        if let Some(apps) = &env.applications {
            for app in s.apps {
                if apps.join(app).is_dir() {
                    signals.push(app.to_string());
                }
            }
        }
        if let Some(home) = &env.home {
            for dir in s.user_dirs {
                if home.join(dir).is_dir() {
                    signals.push(format!("~/{}", dir));
                }
            }
        }
        for dir in s.project_dirs {
            if env.project_root.join(dir).is_dir() {
                signals.push(format!("{}/ in the project", dir));
            }
        }
        if !signals.is_empty() {
            out.push(DetectedTool { tool: tool.to_string(), signals });
        }
    }
    out
}

/// Tools used when nothing is detected: the always-loaded `CLAUDE.md` and `AGENTS.md`, which
/// most coding agents read.
pub const FALLBACK_TOOLS: &[&str] = &["claude", "codex"];

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn exe(dir: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn env(root: &Path) -> DetectEnv {
        DetectEnv {
            path: Some(root.join("bin").into_os_string()),
            home: Some(root.join("home")),
            applications: Some(root.join("Applications")),
            project_root: root.join("proj"),
        }
    }

    #[test]
    fn nothing_detected_in_an_empty_environment() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("proj")).unwrap();
        assert!(detect_tools(&env(d.path())).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn detection_matrix() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        type Case<'a> = (&'a str, &'a dyn Fn(&Path), &'a str);
        let cases: &[Case] = &[
            ("claude", &|r| exe(&r.join("bin"), "claude"), "claude on PATH"),
            ("codex", &|r| exe(&r.join("bin"), "codex"), "codex on PATH"),
            ("cursor", &|r| exe(&r.join("bin"), "cursor"), "cursor on PATH"),
            ("windsurf", &|r| exe(&r.join("bin"), "windsurf"), "windsurf on PATH"),
            ("copilot", &|r| exe(&r.join("bin"), "code"), "code on PATH"),
            ("copilot", &|r| exe(&r.join("bin"), "code-insiders"), "code-insiders on PATH"),
            ("opencode", &|r| exe(&r.join("bin"), "opencode"), "opencode on PATH"),
            ("cursor", &|r| std::fs::create_dir_all(r.join("Applications/Cursor.app")).unwrap(), "Cursor.app"),
            ("windsurf", &|r| std::fs::create_dir_all(r.join("Applications/Windsurf.app")).unwrap(), "Windsurf.app"),
            (
                "copilot",
                &|r| std::fs::create_dir_all(r.join("Applications/Visual Studio Code.app")).unwrap(),
                "Visual Studio Code.app",
            ),
            ("claude", &|r| std::fs::create_dir_all(r.join("home/.claude")).unwrap(), "~/.claude"),
            ("codex", &|r| std::fs::create_dir_all(r.join("home/.codex")).unwrap(), "~/.codex"),
            ("cursor", &|r| std::fs::create_dir_all(r.join("home/.cursor")).unwrap(), "~/.cursor"),
            ("windsurf", &|r| std::fs::create_dir_all(r.join("home/.codeium/windsurf")).unwrap(), "~/.codeium/windsurf"),
            ("opencode", &|r| std::fs::create_dir_all(r.join("home/.config/opencode")).unwrap(), "~/.config/opencode"),
            ("claude", &|r| std::fs::create_dir_all(r.join("proj/.claude")).unwrap(), ".claude/ in the project"),
            ("cursor", &|r| std::fs::create_dir_all(r.join("proj/.cursor")).unwrap(), ".cursor/ in the project"),
            ("copilot", &|r| std::fs::create_dir_all(r.join("proj/.vscode")).unwrap(), ".vscode/ in the project"),
            ("windsurf", &|r| std::fs::create_dir_all(r.join("proj/.windsurf")).unwrap(), ".windsurf/ in the project"),
            ("opencode", &|r| std::fs::create_dir_all(r.join("proj/.opencode")).unwrap(), ".opencode/ in the project"),
        ];
        for (tool, make, signal) in cases {
            let _ = std::fs::remove_dir_all(r);
            std::fs::create_dir_all(r.join("proj")).unwrap();
            make(r);
            let found = detect_tools(&env(r));
            assert_eq!(found.len(), 1, "{}: {:?}", signal, found);
            assert_eq!(found[0].tool, *tool, "{}", signal);
            assert_eq!(found[0].signals, vec![signal.to_string()]);
        }
        // Several tools come back in AI_TOOLS order, with every signal.
        exe(&r.join("bin"), "codex");
        exe(&r.join("bin"), "claude");
        std::fs::create_dir_all(r.join("home/.claude")).unwrap();
        let found = detect_tools(&env(r));
        let ids: Vec<&str> = found.iter().map(|t| t.tool.as_str()).collect();
        assert_eq!(ids, vec!["claude", "opencode", "codex"]);
        assert_eq!(found[0].signals, vec!["claude on PATH", "~/.claude"]);
        // No lookups when a source is absent.
        let none = DetectEnv { path: None, home: None, applications: None, project_root: r.join("elsewhere") };
        assert!(detect_tools(&none).is_empty());
    }
}
