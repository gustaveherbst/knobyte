use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_SCAFFOLD_DIR: &str = ".knobyte";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiConfig {
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnobyteConfigFile {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub scaffold_id: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub project_name: Option<String>,
    #[serde(default)]
    pub wiki: Option<WikiConfig>,
}

fn default_version() -> String {
    crate::version::VERSION.to_string()
}

fn default_mode() -> String {
    "code-repo".to_string()
}

impl Default for KnobyteConfigFile {
    fn default() -> Self {
        Self {
            version: default_version(),
            scaffold_id: Some(Uuid::new_v4().to_string()),
            mode: default_mode(),
            project_name: Some("knobyte".to_string()),
            wiki: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnobyteConfig {
    pub project_root: PathBuf,
    pub scaffold_root: PathBuf,
    pub scaffold_id: String,
    pub mode: String,
    pub project_name: Option<String>,
    pub wiki: WikiConfig,
}

impl KnobyteConfig {
    pub fn new(project_root: PathBuf, scaffold_root: PathBuf) -> Self {
        let (scaffold_id, mode, project_name, wiki) = Self::read_config_file(&scaffold_root);
        let detected = detect_project_name(&project_root);
        let resolved_name = match project_name {
            Some(ref n) if !n.trim().is_empty() && (n != "knobyte" || detected == "knobyte") => Some(n.clone()),
            _ => Some(detected),
        };
        Self {
            project_root,
            scaffold_root,
            scaffold_id,
            mode,
            project_name: resolved_name,
            wiki,
        }
    }

    pub fn project_name(&self) -> String {
        let detected = detect_project_name(&self.project_root);
        if let Some(ref name) = self.project_name {
            if !name.trim().is_empty() && (name != "knobyte" || detected == "knobyte") {
                return name.trim().to_string();
            }
        }
        detected
    }

    fn read_config_file(scaffold_root: &Path) -> (String, String, Option<String>, WikiConfig) {
        let config_file_path = scaffold_root.join("config.json");
        if config_file_path.exists() {
            if let Ok(content) = fs::read_to_string(&config_file_path) {
                if let Ok(parsed) = serde_json::from_str::<KnobyteConfigFile>(&content) {
                    let scaffold_id = parsed.scaffold_id.unwrap_or_else(|| Uuid::new_v4().to_string());
                    let mode = parsed.mode;
                    let project_name = parsed.project_name;
                    let wiki = parsed.wiki.unwrap_or_default();
                    return (scaffold_id, mode, project_name, wiki);
                }
            }
        }
        (Uuid::new_v4().to_string(), "code-repo".to_string(), None, WikiConfig::default())
    }

    pub fn save(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.scaffold_root)?;
        let file = KnobyteConfigFile {
            version: crate::version::VERSION.to_string(),
            scaffold_id: Some(self.scaffold_id.clone()),
            mode: self.mode.clone(),
            project_name: Some(self.project_name()),
            wiki: Some(self.wiki.clone()),
        };
        let content = serde_json::to_string_pretty(&file)?;
        fs::write(self.scaffold_root.join("config.json"), content)?;
        Ok(())
    }

    pub fn graph_db_path(&self) -> PathBuf {
        self.scaffold_root.join("graph.db")
    }

    pub fn wiki_db_path(&self) -> PathBuf {
        self.scaffold_root.join("wiki.db")
    }

    pub fn cozo_db_path(&self) -> PathBuf {
        self.scaffold_root.join("cozo.db")
    }

    pub fn events_dir(&self) -> PathBuf {
        self.scaffold_root.join("events")
    }

    pub fn decisions_log_path(&self) -> PathBuf {
        self.events_dir().join("decisions.jsonl")
    }

    pub fn operations_log_path(&self) -> PathBuf {
        self.events_dir().join("operations.jsonl")
    }

    pub fn activity_dir(&self) -> PathBuf {
        self.events_dir().join("activity")
    }

    pub fn local_dir(&self) -> PathBuf {
        self.scaffold_root.join("local")
    }

    pub fn team_dir(&self) -> PathBuf {
        self.scaffold_root.join("team")
    }

    pub fn members_dir(&self) -> PathBuf {
        self.team_dir().join("members")
    }

    pub fn workstreams_dir(&self) -> PathBuf {
        self.scaffold_root.join("workstreams")
    }

    pub fn specs_dir(&self) -> PathBuf {
        self.scaffold_root.join("specs")
    }

    pub fn inbox_dir(&self) -> PathBuf {
        self.scaffold_root.join("inbox")
    }

    pub fn relays_dir(&self) -> PathBuf {
        self.scaffold_root.join("relays")
    }

    pub fn context_dir(&self) -> PathBuf {
        self.scaffold_root.join("context")
    }

    pub fn patterns_dir(&self) -> PathBuf {
        self.scaffold_root.join("patterns")
    }

    pub fn topics_dir(&self) -> PathBuf {
        self.scaffold_root.join("topics")
    }

    pub fn path_in_scaffold(&self, rel: &str) -> PathBuf {
        self.scaffold_root.join(rel)
    }

    pub fn ensure_scaffold_dirs(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.scaffold_root)?;
        fs::create_dir_all(self.events_dir())?;
        fs::create_dir_all(self.activity_dir())?;
        fs::create_dir_all(self.local_dir())?;
        fs::create_dir_all(self.members_dir())?;
        fs::create_dir_all(self.workstreams_dir())?;
        fs::create_dir_all(self.specs_dir())?;
        fs::create_dir_all(self.inbox_dir())?;
        fs::create_dir_all(self.relays_dir())?;
        fs::create_dir_all(self.context_dir())?;
        fs::create_dir_all(self.patterns_dir())?;
        fs::create_dir_all(self.topics_dir())?;
        Ok(())
    }
}

pub fn find_config(start: Option<&Path>) -> Result<KnobyteConfig, String> {
    let current = match start {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| format!("Failed to get current dir: {}", e))?,
    };

    let mut cursor = current.canonicalize().unwrap_or(current);
    loop {
        let knobyte_dir = cursor.join(DEFAULT_SCAFFOLD_DIR);
        if knobyte_dir.is_dir() {
            return Ok(KnobyteConfig::new(cursor, knobyte_dir));
        }

        let git_dir = cursor.join(".git");
        if git_dir.is_dir() || git_dir.is_file() {
            // Found git root, use .knobyte under it
            let scaffold = cursor.join(DEFAULT_SCAFFOLD_DIR);
            return Ok(KnobyteConfig::new(cursor, scaffold));
        }

        if let Some(parent) = cursor.parent() {
            cursor = parent.to_path_buf();
        } else {
            break;
        }
    }

    // Default to cwd + .knobyte
    let cwd = std::env::current_dir().map_err(|e| format!("Failed to get current dir: {}", e))?;
    let scaffold = cwd.join(DEFAULT_SCAFFOLD_DIR);
    Ok(KnobyteConfig::new(cwd, scaffold))
}

pub fn detect_project_name(project_root: &Path) -> String {
    let cargo_toml = project_root.join("Cargo.toml");
    if cargo_toml.exists() {
        if let Ok(content) = fs::read_to_string(&cargo_toml) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("name =") {
                    let parts: Vec<&str> = trimmed.split('=').collect();
                    if parts.len() == 2 {
                        let n = parts[1].trim().trim_matches('"').trim_matches('\'').trim();
                        if !n.is_empty() {
                            return n.to_string();
                        }
                    }
                }
            }
        }
    }

    let package_json = project_root.join("package.json");
    if package_json.exists() {
        if let Ok(content) = fs::read_to_string(&package_json) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(n) = val.get("name").and_then(|v| v.as_str()) {
                    if !n.trim().is_empty() {
                        return n.trim().to_string();
                    }
                }
            }
        }
    }

    let pyproject = project_root.join("pyproject.toml");
    if pyproject.exists() {
        if let Ok(content) = fs::read_to_string(&pyproject) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("name =") {
                    let parts: Vec<&str> = trimmed.split('=').collect();
                    if parts.len() == 2 {
                        let n = parts[1].trim().trim_matches('"').trim_matches('\'').trim();
                        if !n.is_empty() {
                            return n.to_string();
                        }
                    }
                }
            }
        }
    }

    let go_mod = project_root.join("go.mod");
    if go_mod.exists() {
        if let Ok(content) = fs::read_to_string(&go_mod) {
            for line in content.lines() {
                let trimmed = line.trim();
                if let Some(stripped) = trimmed.strip_prefix("module ") {
                    let mod_path = stripped.trim();
                    let basename = mod_path.rsplit('/').next().unwrap_or(mod_path);
                    if !basename.is_empty() {
                        return basename.to_string();
                    }
                }
            }
        }
    }

    if let Ok(output) = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(project_root)
        .output()
    {
        if output.status.success() {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let repo_name = url
                .trim_end_matches(".git")
                .rsplit(['/', ':'])
                .next()
                .unwrap_or("")
                .trim();
            if !repo_name.is_empty() {
                return repo_name.to_string();
            }
        }
    }

    project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string()
}

