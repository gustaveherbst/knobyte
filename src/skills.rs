//! Official Knobyte agent skills for Claude Code (`.claude/skills/`) and Codex
//! (`.agents/skills/`), plus the managed instruction block in the root `CLAUDE.md` /
//! `AGENTS.md` that points the agent at the scaffold and the skills.
//!
//! Installed skill directories carry `.knobyte-managed.json` with the SHA-256 of every file
//! Knobyte wrote. A directory is only replaced when its files still match those hashes; a
//! user-edited or unmanaged directory is reported as a conflict and never overwritten
//! (`--backup` moves it aside under `.knobyte/local/skill-backups/` first).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::drift::checkers::tool_config_sync::{SKILLS_END, SKILLS_START};
use crate::managed_block::{plan_block_edit, sha256_hex, BlockAction, BlockReason, BlockSpec};

/// Agent clients for which skills can be synced.
pub const SKILL_TOOLS: &[&str] = &["claude", "codex"];

/// Ownership metadata file inside each installed skill directory.
pub const MANAGED_METADATA: &str = ".knobyte-managed.json";
pub const MANAGED_SCHEMA_VERSION: u32 = 1;

/// The official skills.
pub const OFFICIAL_SKILLS: &[&str] = &["knobyte-inbox", "knobyte-relay"];

/// (skill, skill-relative path, content)
const SKILL_FILES: &[(&str, &str, &str)] = &[
    ("knobyte-inbox", "SKILL.md", include_str!("../templates/skills/knobyte-inbox/SKILL.md")),
    ("knobyte-inbox", "agents/openai.yaml", include_str!("../templates/skills/knobyte-inbox/agents/openai.yaml")),
    (
        "knobyte-inbox",
        "references/cli-workflows.md",
        include_str!("../templates/skills/knobyte-inbox/references/cli-workflows.md"),
    ),
    ("knobyte-relay", "SKILL.md", include_str!("../templates/skills/knobyte-relay/SKILL.md")),
    ("knobyte-relay", "agents/openai.yaml", include_str!("../templates/skills/knobyte-relay/agents/openai.yaml")),
    (
        "knobyte-relay",
        "references/cli-workflows.md",
        include_str!("../templates/skills/knobyte-relay/references/cli-workflows.md"),
    ),
];

/// Single-file `SKILL.md` bodies written verbatim (without ownership metadata) by earlier
/// Knobyte versions; such a directory is upgraded in place.
const LEGACY_INBOX_SKILL: &str = "---\nname: knobyte-inbox\ndescription: Propose an addition or correction to project knowledge in Knobyte\n---\n\n# Knobyte Inbox Skill\n\nUse this skill when you make a discovery, decide on an architecture pattern, or need to correct existing documentation.\n\n1. Save a checkout-local draft. `--target` is a Markdown path relative to `.knobyte/`\n   (for example `context/rate-limit.md`); `--reason` is required:\n\n   `knobyte inbox draft save --title \"<title>\" --target \"context/<doc>.md\" --content \"<markdown>\" --reason \"<why this matters>\" [--mode append|replace]`\n\n   `--mode append` (default) appends to an existing document; `--mode replace` replaces it.\n2. Show the draft to the human (`knobyte inbox draft list`). Publish only when asked:\n   `knobyte inbox publish <draft-id>`.\n\nApproving or rejecting proposals (`knobyte inbox approve <id>` / `knobyte inbox reject <id>`)\nis a human-only review decision. Never approve or reject a proposal yourself.\n";
const LEGACY_RELAY_SKILL: &str = "---\nname: knobyte-relay\ndescription: Package context, progress, and next actions as a durable handoff in Knobyte\n---\n\n# Knobyte Relay Skill\n\nUse this skill when completing a session or passing context to another engineer or agent.\n\nSave a checkout-local draft (all list flags are repeatable):\n\n`knobyte relay draft save --title \"<title>\" --summary \"<summary>\" [--to <member-id>]... [--progress \"<done>\"]... [--blocker \"<blocker>\"]... [--next \"<next action>\"]... [--evidence \"<file, test or command>\"]...`\n\n- `--to` addresses named teammates; without it the relay is open to the whole team.\n- `--sender` defaults to the current member (`knobyte member current`).\n- Branch, HEAD commit and dirty-tree state are captured automatically on publish.\n\nShow the draft to the human (`knobyte relay draft list`) and publish only when asked:\n`knobyte relay publish <draft-id>`. Inspect a relay with `knobyte relay show <relay-id>`.\n";

fn is_legacy_skill(content: &[u8]) -> bool {
    let h = sha256_hex(content);
    h == sha256_hex(LEGACY_INBOX_SKILL.as_bytes())
        || h == sha256_hex(LEGACY_RELAY_SKILL.as_bytes())
}

/// Refuse to edit an existing managed instruction block larger than this.
const MAX_INSTRUCTION_BLOCK_BYTES: usize = 32 * 1024;

struct Target {
    skills_dir: &'static str,
    instructions: &'static str,
    prefix: &'static str,
}

fn target(client: &str) -> Target {
    match client {
        "claude" => Target { skills_dir: ".claude/skills", instructions: "CLAUDE.md", prefix: "/" },
        _ => Target { skills_dir: ".agents/skills", instructions: "AGENTS.md", prefix: "$" },
    }
}

/// The managed instruction block for `client`.
pub fn render_instruction_block(client: &str, eol: &str) -> String {
    let t = target(client);
    let inbox = format!("{}knobyte-inbox", t.prefix);
    let relay = format!("{}knobyte-relay", t.prefix);
    [
        SKILLS_START.to_string(),
        "## Knobyte agent skills".to_string(),
        "- At the start of every session, read `.knobyte/AGENTS.md` and `.knobyte/ROUTER.md` before project work; follow `ROUTER.md` to load only the relevant context.".to_string(),
        "- Read `knobyte logging --json` at session start and before optional logging: `significant` (default: material decisions, risks, blockers, durable discoveries), `checkpoints` (batch notes at task or session boundaries) or `manual` (no unsolicited notes). Always honor explicit user log requests.".to_string(),
        "- When earlier work may inform the task, search history with `knobyte timeline` and treat matches as historical evidence, not accepted current knowledge.".to_string(),
        format!("- Use `{}` for explicit contributions to project knowledge and `{}` for durable team handoffs. Invoke them when intent clearly matches; ordinary GROW upkeep needs no Inbox.", inbox, relay),
        "- When Knobyte context materially helps, mention the relevant finding naturally; do not narrate routine context loading.".to_string(),
        "- Do not claim an author, date, or historical event unless the retrieved data actually provides it.".to_string(),
        "- After a Knobyte write, say exactly what changed and its sharing boundary: a local draft is checkout-only; a canonical artifact is written to the working tree and needs commit and push to be shared.".to_string(),
        "- Skill activation is not approval for canonical actions.".to_string(),
        SKILLS_END.to_string(),
    ]
    .join(eol)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSyncAction {
    pub client: String,
    /// Skill name, or `instructions` for the managed block in CLAUDE.md / AGENTS.md.
    pub skill_name: String,
    /// Absolute path.
    pub path: String,
    /// `create`, `update`, `unchanged`, `conflict` or `backup`.
    pub action: String,
    /// `skill` or `instructions`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillWarning {
    pub code: String,
    pub client: String,
    pub path: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSyncReport {
    /// Clients synced (`claude`, `codex`).
    #[serde(default)]
    pub clients: Vec<String>,
    pub dry_run: bool,
    pub actions: Vec<SkillSyncAction>,
    #[serde(default)]
    pub warnings: Vec<SkillWarning>,
    /// At least one target was left untouched because it could not be updated safely.
    #[serde(default)]
    pub conflicted: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SkillSyncOptions {
    pub dry_run: bool,
    /// Check that installed skill files are not ignored by git (code repositories).
    pub check_ignored: bool,
    /// Move conflicting skill directories aside (under `.knobyte/local/skill-backups/`) and
    /// install fresh copies instead of reporting a conflict.
    pub backup_conflicts: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ManagedMetadata {
    schema_version: u32,
    owner: String,
    skill: String,
    version: String,
    files: BTreeMap<String, String>,
}

fn desired_files(skill: &str) -> BTreeMap<String, &'static str> {
    SKILL_FILES.iter().filter(|(s, _, _)| *s == skill).map(|(_, p, c)| (p.to_string(), *c)).collect()
}

fn desired_hashes(skill: &str) -> BTreeMap<String, String> {
    desired_files(skill).into_iter().map(|(p, c)| (p, sha256_hex(c.as_bytes()))).collect()
}

/// Every regular file under `dir` (except the metadata file) with its hash; `Err` names an
/// unsafe entry (symlink or special file).
fn snapshot(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for entry in walkdir::WalkDir::new(dir).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|e| e.to_string())?;
        let rel = entry.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
        let ft = entry.file_type();
        if ft.is_symlink() || !(ft.is_file() || ft.is_dir()) {
            return Err(rel);
        }
        if ft.is_file() && rel != MANAGED_METADATA {
            let bytes = fs::read(entry.path()).map_err(|e| e.to_string())?;
            out.insert(rel, sha256_hex(&bytes));
        }
    }
    Ok(out)
}

/// A path component under the project root that is a symlink, if any.
fn unsafe_component(project_root: &Path, rel: &str) -> Option<String> {
    let mut p = project_root.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
        if let Ok(m) = fs::symlink_metadata(&p) {
            if m.file_type().is_symlink() {
                return Some(p.to_string_lossy().to_string());
            }
        } else {
            return None;
        }
    }
    None
}

enum SkillPlan {
    Create,
    Update { remove: Vec<String> },
    Unchanged,
    Conflict { code: &'static str, message: String, resolution: String },
}

fn plan_skill(project_root: &Path, rel_dir: &str, skill: &str) -> SkillPlan {
    if let Some(link) = unsafe_component(project_root, rel_dir) {
        return SkillPlan::Conflict {
            code: "unsafe-path",
            message: format!("{} is a symlink, so {} was not touched.", link, rel_dir),
            resolution: "Replace the symlink with a real directory, then run knobyte skills sync again.".into(),
        };
    }
    let dir = project_root.join(rel_dir);
    let Ok(meta) = fs::symlink_metadata(&dir) else {
        return SkillPlan::Create;
    };
    let unmanaged = |what: String| SkillPlan::Conflict {
        code: "unmanaged-skill-conflict",
        message: what,
        resolution: format!(
            "Move or rename {} (or rerun with --backup to move it aside automatically). Knobyte never overwrites it.",
            rel_dir
        ),
    };
    if !meta.is_dir() {
        return unmanaged(format!("{} exists and is not a Knobyte-managed skill directory.", rel_dir));
    }
    let snap = match snapshot(&dir) {
        Ok(s) => s,
        Err(entry) => {
            return SkillPlan::Conflict {
                code: "unsafe-path",
                message: format!("{} contains a symlink or special file at {}.", rel_dir, entry),
                resolution: "Remove the unsafe entry yourself, then run knobyte skills sync again.".into(),
            }
        }
    };
    let desired = desired_hashes(skill);
    let metadata_path = dir.join(MANAGED_METADATA);
    let Ok(raw) = fs::read(&metadata_path) else {
        // Single-file skills written verbatim by earlier versions are upgraded in place.
        if snap.len() == 1 {
            if let Ok(c) = fs::read(dir.join("SKILL.md")) {
                if is_legacy_skill(&c) {
                    return SkillPlan::Update { remove: vec!["SKILL.md".into()] };
                }
            }
        }
        return unmanaged(format!("{} already exists without {}.", rel_dir, MANAGED_METADATA));
    };
    let parsed: Option<ManagedMetadata> = serde_json::from_slice(&raw).ok();
    let Some(md) = parsed.filter(|m| m.owner == "knobyte" && m.skill == skill && m.schema_version == MANAGED_SCHEMA_VERSION) else {
        return SkillPlan::Conflict {
            code: "malformed-ownership",
            message: format!("{}/{} is malformed or does not identify this skill.", rel_dir, MANAGED_METADATA),
            resolution: format!("Restore the ownership file or move {} aside (--backup); Knobyte will not guess ownership.", rel_dir),
        };
    };
    if md.files != snap {
        let changed: Vec<String> = snap
            .iter()
            .filter(|(p, h)| md.files.get(*p) != Some(*h))
            .map(|(p, _)| p.clone())
            .chain(md.files.keys().filter(|p| !snap.contains_key(*p)).cloned())
            .collect();
        return SkillPlan::Conflict {
            code: "managed-skill-modified",
            message: format!("{} has local changes ({}) relative to its recorded hashes.", rel_dir, changed.join(", ")),
            resolution: "Preserve or move your changes (or rerun with --backup), then sync again. Knobyte will not overwrite modified files.".into(),
        };
    }
    if md.files == desired {
        SkillPlan::Unchanged
    } else {
        SkillPlan::Update { remove: md.files.keys().cloned().collect() }
    }
}

fn write_skill(dir: &Path, skill: &str, remove: &[String]) -> Result<(), String> {
    for rel in remove {
        let p = dir.join(rel);
        if p.is_file() {
            fs::remove_file(&p).map_err(|e| format!("{}: {}", p.display(), e))?;
        }
    }
    let _ = fs::remove_file(dir.join(MANAGED_METADATA));
    for (rel, content) in desired_files(skill) {
        let p = dir.join(&rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
        }
        fs::write(&p, content).map_err(|e| format!("{}: {}", p.display(), e))?;
    }
    // Drop directories emptied by removed files.
    for rel in remove {
        if let Some(parent) = dir.join(rel).parent() {
            if parent != dir {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    let md = ManagedMetadata {
        schema_version: MANAGED_SCHEMA_VERSION,
        owner: "knobyte".into(),
        skill: skill.into(),
        version: crate::version::VERSION.into(),
        files: desired_hashes(skill),
    };
    let json = serde_json::to_string_pretty(&md).map_err(|e| e.to_string())?;
    fs::write(dir.join(MANAGED_METADATA), format!("{}\n", json)).map_err(|e| e.to_string())
}

fn backup_dir(config: &KnobyteConfig, client: &str, skill: &str) -> PathBuf {
    config
        .local_dir()
        .join("skill-backups")
        .join(format!("{}-{}-{}", client, skill, chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")))
}

/// Whether git ignores `rel` (Ok(None) when the project is not a git repository).
fn git_ignored(project_root: &Path, rel: &str) -> Result<Option<bool>, String> {
    if !project_root.join(".git").exists() {
        return Ok(None);
    }
    let out = Command::new("git")
        .args(["check-ignore", "-q", "--no-index", "--", rel])
        .current_dir(project_root)
        .output()
        .map_err(|e| e.to_string())?;
    match out.status.code() {
        Some(0) => Ok(Some(true)),
        Some(1) => Ok(Some(false)),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

fn narrow_ignore_resolution(skills_dir: &str) -> String {
    let client_dir = skills_dir.split('/').next().unwrap_or(skills_dir);
    let mut lines = vec![
        format!("!/{}/", client_dir),
        format!("/{}/*", client_dir),
        format!("!/{}/", skills_dir),
        format!("/{}/*", skills_dir),
    ];
    for s in OFFICIAL_SKILLS {
        lines.push(format!("!/{}/{}/", skills_dir, s));
        lines.push(format!("!/{}/{}/**", skills_dir, s));
    }
    format!(
        "Add these rules after the broad ignore rule in the root .gitignore:\n{}\nThey re-open only the official skills; Knobyte never edits ignore rules automatically.",
        lines.join("\n")
    )
}

/// Clients to sync: the explicit `--tool`, else the agents selected during setup
/// (`aiTools` in config.json). Refuses when neither names a supported agent.
fn parse_clients(config: &KnobyteConfig, tool: Option<&str>) -> Result<Vec<&'static str>, String> {
    match tool.map(|t| t.trim().to_ascii_lowercase()) {
        None => {
            let configured = crate::config::load_ai_tools(&config.scaffold_root).unwrap_or_default();
            let clients: Vec<&'static str> =
                SKILL_TOOLS.iter().copied().filter(|c| configured.iter().any(|t| t == c)).collect();
            if clients.is_empty() {
                Err("No supported agent is selected. Run `knobyte setup` or pass --tool claude, --tool codex or --tool all.".to_string())
            } else {
                Ok(clients)
            }
        }
        Some(t) if t == "all" => Ok(SKILL_TOOLS.to_vec()),
        Some(t) => match SKILL_TOOLS.iter().find(|c| **c == t) {
            Some(c) => Ok(vec![*c]),
            None => Err(format!("Unknown tool '{}'. Valid tools: {}, all", t, SKILL_TOOLS.join(", "))),
        },
    }
}

/// Write (or preview) the Knobyte agent skills. `tool` selects one client (`claude` or
/// `codex`) or `all`; `None` syncs the agents selected during setup.
pub fn sync_skills(config: &KnobyteConfig, tool: Option<&str>, dry_run: bool) -> Result<SkillSyncReport, String> {
    let clients = parse_clients(config, tool)?;
    sync_agent_assets(config, &clients, SkillSyncOptions { dry_run, check_ignored: true, backup_conflicts: false })
}

/// Same as [`sync_skills`] with explicit options.
pub fn sync_skills_with(config: &KnobyteConfig, tool: Option<&str>, opts: SkillSyncOptions) -> Result<SkillSyncReport, String> {
    let clients = parse_clients(config, tool)?;
    sync_agent_assets(config, &clients, opts)
}

/// Install the official skills and the managed instruction block for `clients`.
pub fn sync_agent_assets(config: &KnobyteConfig, clients: &[&str], opts: SkillSyncOptions) -> Result<SkillSyncReport, String> {
    let root = &config.project_root;
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let mut conflicted = false;

    for client in SKILL_TOOLS.iter().filter(|c| clients.contains(c)) {
        let t = target(client);
        for skill in OFFICIAL_SKILLS {
            let rel_dir = format!("{}/{}", t.skills_dir, skill);
            let dir = root.join(&rel_dir);
            let path = dir.join("SKILL.md").to_string_lossy().to_string();
            let mut push = |action: &str, message: String| {
                actions.push(SkillSyncAction {
                    client: client.to_string(),
                    skill_name: skill.to_string(),
                    path: path.clone(),
                    action: action.to_string(),
                    kind: "skill".into(),
                    message,
                })
            };
            match plan_skill(root, &rel_dir, skill) {
                SkillPlan::Unchanged => push("unchanged", format!("{} already matches the packaged {} skill.", rel_dir, skill)),
                SkillPlan::Create => {
                    if !opts.dry_run {
                        write_skill(&dir, skill, &[])?;
                    }
                    push("create", format!("Install the {} skill at {}.", skill, rel_dir));
                }
                SkillPlan::Update { remove } => {
                    if !opts.dry_run {
                        write_skill(&dir, skill, &remove)?;
                    }
                    push("update", format!("Update the unmodified Knobyte-managed {} skill at {}.", skill, rel_dir));
                }
                SkillPlan::Conflict { code, message, resolution } => {
                    if opts.backup_conflicts && code != "unsafe-path" {
                        let dest = backup_dir(config, client, skill);
                        if !opts.dry_run {
                            fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
                            fs::rename(&dir, &dest).map_err(|e| format!("could not back up {}: {}", rel_dir, e))?;
                            write_skill(&dir, skill, &[])?;
                        }
                        push("backup", format!("Moved {} to {} and installed a fresh copy.", rel_dir, dest.display()));
                    } else {
                        conflicted = true;
                        push("conflict", message.clone());
                        warnings.push(SkillWarning {
                            code: code.into(),
                            client: client.to_string(),
                            path: rel_dir.clone(),
                            message,
                            resolution: Some(resolution),
                        });
                    }
                }
            }
            if opts.check_ignored {
                let rel_file = format!("{}/SKILL.md", rel_dir);
                match git_ignored(root, &rel_file) {
                    Ok(Some(true)) => warnings.push(SkillWarning {
                        code: "ignored-skill-path".into(),
                        client: client.to_string(),
                        path: rel_file.clone(),
                        message: format!("{} is ignored by git, so teammates will not receive the {} skill.", rel_file, skill),
                        resolution: Some(narrow_ignore_resolution(t.skills_dir)),
                    }),
                    Ok(_) => {}
                    Err(e) => warnings.push(SkillWarning {
                        code: "ignore-check-failed".into(),
                        client: client.to_string(),
                        path: rel_file.clone(),
                        message: format!("Could not verify whether {} is ignored by git: {}", rel_file, e),
                        resolution: None,
                    }),
                }
            }
        }

        // Managed instruction block in CLAUDE.md / AGENTS.md.
        let ipath = root.join(t.instructions);
        let ipath_s = ipath.to_string_lossy().to_string();
        let current = fs::read(&ipath).ok();
        let client_id = *client;
        let render = move |eol: &str| render_instruction_block(client_id, eol);
        let spec = BlockSpec {
            start: SKILLS_START,
            end: SKILLS_END,
            render: &render,
            max_block_bytes: MAX_INSTRUCTION_BLOCK_BYTES,
            legacy_hashes: &[],
            is_already_pointing: None,
        };
        let edit = plan_block_edit(&spec, current.as_deref());
        let (action, message) = match edit.action {
            BlockAction::Noop => ("unchanged", format!("{} already contains the exact managed block.", t.instructions)),
            BlockAction::Create => ("create", format!("Create {} with the managed Knobyte instruction block.", t.instructions)),
            BlockAction::Migrate => ("update", format!("Migrate the legacy Knobyte {} to the managed block.", t.instructions)),
            BlockAction::Update if edit.reason == BlockReason::Append => (
                "update",
                format!("Append the managed Knobyte block to {} without changing its existing bytes.", t.instructions),
            ),
            BlockAction::Update => ("update", format!("Replace only the managed Knobyte block in {}.", t.instructions)),
            BlockAction::Conflict => {
                conflicted = true;
                let (code, msg) = match edit.reason {
                    BlockReason::InvalidEncoding => (
                        "invalid-instruction-encoding",
                        format!("{} is not valid UTF-8, so its bytes were preserved.", t.instructions),
                    ),
                    BlockReason::TooLarge => (
                        "malformed-instruction-markers",
                        format!("{} has a managed block larger than the safe edit limit.", t.instructions),
                    ),
                    _ => (
                        "malformed-instruction-markers",
                        format!("{} has duplicate, nested, partial or non-standalone Knobyte markers.", t.instructions),
                    ),
                };
                warnings.push(SkillWarning {
                    code: code.into(),
                    client: client.to_string(),
                    path: t.instructions.into(),
                    message: msg.clone(),
                    resolution: Some(format!(
                        "Repair the {} / {} markers in {} yourself, then run knobyte skills sync again.",
                        SKILLS_START, SKILLS_END, t.instructions
                    )),
                });
                ("conflict", msg)
            }
        };
        if !opts.dry_run {
            if let Some(bytes) = &edit.desired {
                fs::write(&ipath, bytes).map_err(|e| format!("{}: {}", ipath.display(), e))?;
            }
        }
        actions.push(SkillSyncAction {
            client: client.to_string(),
            skill_name: "instructions".into(),
            path: ipath_s,
            action: action.into(),
            kind: "instructions".into(),
            message,
        });
    }

    Ok(SkillSyncReport {
        clients: clients.iter().map(|c| c.to_string()).collect(),
        dry_run: opts.dry_run,
        actions,
        warnings,
        conflicted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> KnobyteConfig {
        KnobyteConfig::new(dir.to_path_buf(), dir.join(".knobyte"))
    }

    #[test]
    fn install_then_noop_then_user_edit_is_never_clobbered() {
        let d = tempfile::tempdir().unwrap();
        let c = config(d.path());
        let r = sync_skills(&c, Some("claude"), false).unwrap();
        assert!(r.actions.iter().all(|a| a.action == "create"));
        let skill = d.path().join(".claude/skills/knobyte-inbox");
        assert!(skill.join(MANAGED_METADATA).is_file());
        assert!(skill.join("references/cli-workflows.md").is_file());
        assert!(fs::read_to_string(d.path().join("CLAUDE.md")).unwrap().contains(".knobyte/ROUTER.md"));

        let again = sync_skills(&c, Some("claude"), false).unwrap();
        assert!(again.actions.iter().all(|a| a.action == "unchanged"), "{:?}", again.actions);

        fs::write(skill.join("SKILL.md"), "my edits").unwrap();
        let r = sync_skills(&c, Some("claude"), false).unwrap();
        assert!(r.conflicted);
        assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "my edits");
        assert!(r.warnings.iter().any(|w| w.code == "managed-skill-modified"));

        let r = sync_skills_with(&c, Some("claude"), SkillSyncOptions { backup_conflicts: true, ..Default::default() }).unwrap();
        assert!(!r.conflicted);
        assert!(r.actions.iter().any(|a| a.action == "backup"));
        assert_ne!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "my edits");
        let backups = fs::read_dir(c.local_dir().join("skill-backups")).unwrap().count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn unmanaged_dir_conflicts_and_legacy_upgrades() {
        let d = tempfile::tempdir().unwrap();
        let c = config(d.path());
        let own = d.path().join(".agents/skills/knobyte-relay");
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join("SKILL.md"), "hand written").unwrap();
        let legacy = d.path().join(".agents/skills/knobyte-inbox");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("SKILL.md"), LEGACY_INBOX_SKILL).unwrap();
        fs::write(d.path().join("AGENTS.md"), "# Mine\r\nrules\r\n").unwrap();

        let r = sync_skills(&c, Some("codex"), false).unwrap();
        let by = |s: &str| r.actions.iter().find(|a| a.skill_name == s).unwrap().action.clone();
        assert_eq!(by("knobyte-relay"), "conflict");
        assert_eq!(by("knobyte-inbox"), "update");
        assert_eq!(fs::read_to_string(own.join("SKILL.md")).unwrap(), "hand written");
        let agents = fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("# Mine\r\nrules\r\n\r\n<!-- knobyte-agent:skills:start -->\r\n"));
        assert!(agents.contains("$knobyte-inbox"));
    }
}
