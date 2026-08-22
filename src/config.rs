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

/// Embedding backend used for the CozoDB vector indices.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingBackend {
    /// Deterministic hashed lexical embedding (128-dim, no model needed).
    #[default]
    Hashed,
    /// Local Model2Vec static embedding model (downloaded with `knobyte cozo model pull`).
    Model2vec,
}

impl EmbeddingBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbeddingBackend::Hashed => "hashed",
            EmbeddingBackend::Model2vec => "model2vec",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hashed" => Some(EmbeddingBackend::Hashed),
            "model2vec" => Some(EmbeddingBackend::Model2vec),
            _ => None,
        }
    }
}

/// Default Model2Vec model (Hugging Face repo id).
pub const DEFAULT_EMBEDDING_MODEL: &str = "minishlab/potion-base-8M";

/// `embedding` section of `.knobyte/config.json`. Older configs have no such section and
/// therefore keep the hashed backend.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EmbeddingConfig {
    #[serde(default)]
    pub backend: EmbeddingBackend,
    /// Hugging Face repo id of the Model2Vec model (model2vec backend only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl EmbeddingConfig {
    /// The configured model repo, or the default one.
    pub fn model_repo(&self) -> String {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(DEFAULT_EMBEDDING_MODEL)
            .to_string()
    }
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
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    /// `graph` section (corpus policy for the code graph). Preserved verbatim on save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<GraphConfig>,
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
            embedding: EmbeddingConfig::default(),
            graph: None,
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
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

impl KnobyteConfig {
    pub fn new(project_root: PathBuf, scaffold_root: PathBuf) -> Self {
        let (scaffold_id, mode, project_name, wiki, embedding) =
            Self::read_config_file(&scaffold_root);
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
            embedding,
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

    fn read_config_file(
        scaffold_root: &Path,
    ) -> (String, String, Option<String>, WikiConfig, EmbeddingConfig) {
        let config_file_path = scaffold_root.join("config.json");
        if config_file_path.exists() {
            if let Ok(content) = fs::read_to_string(&config_file_path) {
                if let Ok(parsed) = serde_json::from_str::<KnobyteConfigFile>(&content) {
                    let scaffold_id = parsed.scaffold_id.unwrap_or_else(|| Uuid::new_v4().to_string());
                    let mode = parsed.mode;
                    let project_name = parsed.project_name;
                    let wiki = parsed.wiki.unwrap_or_default();
                    return (scaffold_id, mode, project_name, wiki, parsed.embedding);
                }
            }
        }
        (
            Uuid::new_v4().to_string(),
            "code-repo".to_string(),
            None,
            WikiConfig::default(),
            EmbeddingConfig::default(),
        )
    }

    /// Serialized `config.json` content for this configuration. Keys this struct does not model
    /// (`aiTools`, `heartbeat`, `watch`, `staleness_thresholds`, ...) are carried over from the
    /// existing file unchanged, and known `wiki` keys beyond `exclude`/`read_only` survive too.
    pub fn config_file_json(&self) -> std::io::Result<String> {
        let file = KnobyteConfigFile {
            version: crate::version::VERSION.to_string(),
            scaffold_id: Some(self.scaffold_id.clone()),
            mode: self.mode.clone(),
            project_name: Some(self.project_name()),
            wiki: Some(self.wiki.clone()),
            embedding: self.embedding.clone(),
            graph: read_graph_config_section(&self.scaffold_root),
        };
        let mut value = serde_json::to_value(&file)?;
        if let Some(serde_json::Value::Object(existing)) = read_config_value(&self.scaffold_root) {
            if let serde_json::Value::Object(ref mut map) = value {
                for (k, v) in existing {
                    match map.get_mut(&k) {
                        None => {
                            map.insert(k, v);
                        }
                        // `graph` keys this struct does not model (`typescript`) survive too.
                        Some(serde_json::Value::Object(known)) if k == "wiki" || k == "graph" => {
                            if let serde_json::Value::Object(old) = v {
                                for (wk, wv) in old {
                                    known.entry(wk).or_insert(wv);
                                }
                            }
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        Ok(serde_json::to_string_pretty(&value)?)
    }

    pub fn config_file_path(&self) -> PathBuf {
        self.scaffold_root.join("config.json")
    }

    /// Set the embedding configuration and persist only the `embedding` section of
    /// `config.json`, leaving every other field of an existing file untouched.
    pub fn save_embedding(&mut self, embedding: EmbeddingConfig) -> std::io::Result<()> {
        self.embedding = embedding;
        let path = self.config_file_path();
        let existing = fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .filter(|v| v.is_object());
        let content = match existing {
            Some(mut value) => {
                value["embedding"] = serde_json::to_value(&self.embedding)?;
                serde_json::to_string_pretty(&value)?
            }
            None => self.config_file_json()?,
        };
        fs::create_dir_all(&self.scaffold_root)?;
        fs::write(path, content)
    }

    pub fn save(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.scaffold_root)?;
        let content = self.config_file_json()?;
        fs::write(self.config_file_path(), content)?;
        Ok(())
    }

    /// All directories that make up the scaffold layout.
    pub fn scaffold_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.scaffold_root.clone(),
            self.events_dir(),
            self.activity_dir(),
            self.local_dir(),
            self.team_dir(),
            self.members_dir(),
            self.workstreams_dir(),
            self.specs_dir(),
            self.inbox_dir(),
            self.relays_dir(),
            self.context_dir(),
            self.patterns_dir(),
            self.topics_dir(),
        ]
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
        for dir in self.scaffold_dirs() {
            fs::create_dir_all(dir)?;
        }
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

/// The nearest ancestor of `start` (inclusive) holding a `.git` directory or file.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    start.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// Why a command that needs a Knobyte project cannot run here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectProblem {
    /// Run from inside the `.knobyte/` directory itself.
    InsideScaffold,
    /// No scaffold, and no repository to create one in.
    NoGitRepository,
    /// A repository without a `.knobyte/` scaffold.
    ScaffoldMissing,
    /// `.knobyte/` exists but setup never completed (no `ROUTER.md`).
    ScaffoldIncomplete,
}

impl ProjectProblem {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InsideScaffold => "inside_scaffold",
            Self::NoGitRepository => "not_git_repository",
            Self::ScaffoldMissing => "scaffold_missing",
            Self::ScaffoldIncomplete => "scaffold_incomplete",
        }
    }
}

impl std::fmt::Display for ProjectProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InsideScaffold => {
                "You're inside the .knobyte/ directory. Run knobyte commands from your project root instead."
            }
            Self::NoGitRepository => "No git repository found. Initialize one first (`git init`), then run `knobyte setup`.",
            Self::ScaffoldMissing => "No .knobyte/ scaffold found — run `knobyte setup`.",
            Self::ScaffoldIncomplete => {
                "The .knobyte/ scaffold is incomplete (ROUTER.md is missing) — run `knobyte setup`."
            }
        })
    }
}

impl std::error::Error for ProjectProblem {}

/// Verify that `config` (from [`find_config`]) points at a complete Knobyte project.
/// Commands that read or write the scaffold call this first so they never run against,
/// or create files in, a directory that was never set up.
pub fn require_project(config: &KnobyteConfig) -> Result<(), ProjectProblem> {
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.components().any(|c| c.as_os_str() == DEFAULT_SCAFFOLD_DIR) {
            return Err(ProjectProblem::InsideScaffold);
        }
    }
    if config.scaffold_root.is_dir() {
        if config.scaffold_root.join("ROUTER.md").is_file() {
            return Ok(());
        }
        return Err(ProjectProblem::ScaffoldIncomplete);
    }
    if find_git_root(&config.project_root).is_none() {
        return Err(ProjectProblem::NoGitRepository);
    }
    Err(ProjectProblem::ScaffoldMissing)
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

    if let Ok(content) = fs::read_to_string(project_root.join("Package.swift")) {
        if let Some(n) = crate::scanner::parse_package_swift(&content).name.filter(|n| !n.trim().is_empty()) {
            return n.trim().to_string();
        }
    }

    if let Some(n) = crate::scanner::xcode_project_name(project_root) {
        return n;
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
            // A clone of a bare `origin.git` (or `remote.git`) carries no project name.
            if !repo_name.is_empty() && !matches!(repo_name, "origin" | "remote" | "upstream") {
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

// ---------------------------------------------------------------------------------------------
// Drift-check settings (`knobyte check`). Kept separate from `KnobyteConfigFile` and read on
// demand so other config sections are unaffected. Both snake_case and camelCase keys are
// accepted in `.knobyte/config.json`:
//
// { "staleness_thresholds": { "warn_days": 30, "error_days": 90, "warn_commits": 50, "error_commits": 200 } }
// ---------------------------------------------------------------------------------------------

/// Thresholds for the `STALE_FILE` drift checker (git age / commits since a scaffold file
/// last changed, and the age of its `last_updated` frontmatter date).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StalenessThresholds {
    /// Days since last change that trigger a warning.
    #[serde(default = "default_stale_warn_days", alias = "warnDays")]
    pub warn_days: i64,
    /// Days since last change that trigger an error.
    #[serde(default = "default_stale_error_days", alias = "errorDays")]
    pub error_days: i64,
    /// Commits since last change that trigger a warning.
    #[serde(default = "default_stale_warn_commits", alias = "warnCommits")]
    pub warn_commits: i64,
    /// Commits since last change that trigger an error.
    #[serde(default = "default_stale_error_commits", alias = "errorCommits")]
    pub error_commits: i64,
}

fn default_stale_warn_days() -> i64 {
    30
}
fn default_stale_error_days() -> i64 {
    90
}
fn default_stale_warn_commits() -> i64 {
    50
}
fn default_stale_error_commits() -> i64 {
    200
}

impl Default for StalenessThresholds {
    fn default() -> Self {
        Self {
            warn_days: default_stale_warn_days(),
            error_days: default_stale_error_days(),
            warn_commits: default_stale_warn_commits(),
            error_commits: default_stale_error_commits(),
        }
    }
}

/// Drift-check section of `.knobyte/config.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriftSettings {
    #[serde(default, alias = "stalenessThresholds")]
    pub staleness_thresholds: StalenessThresholds,
}

impl DriftSettings {
    /// Read the drift settings from `<scaffold_root>/config.json`; defaults when the file or
    /// the section is absent or malformed.
    pub fn load(scaffold_root: &Path) -> Self {
        fs::read_to_string(scaffold_root.join("config.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<DriftSettings>(&c).ok())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Wiki scope, write protection and synthesis knobs (`wiki` section of config.json).
//
// Kept separate from `WikiConfig` above so older configs keep deserializing unchanged. Every
// field has a default; malformed values fall back to the default instead of failing the load.
//
// ```json
// "wiki": {
//   "exclude": ["**/node_modules/**"],
//   "readOnly": ["imported/**"],
//   "entityTypes": ["runbook"],
//   "synthesis": { "minFiles": 1, "maxTokens": 4000 }
// }
// ```
// ---------------------------------------------------------------------------

/// Team-owned scaffold paths the wiki never writes, whatever `wiki.readOnly` says.
pub const WIKI_TEAM_OWNED_READ_ONLY: [&str; 5] = [
    "team/**",
    "workstreams/**",
    "inbox/**",
    "relays/**",
    "events/activity/**",
];

/// Default `wiki.exclude`.
pub const WIKI_DEFAULT_EXCLUDE: [&str; 1] = ["**/node_modules/**"];

/// Scope knobs for wiki synthesis. They change what synthesis looks at, never what it accepts
/// (confidence gates are constants).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct WikiSynthesisConfig {
    /// Source files a folder needs before it is worth proposing knowledge about.
    pub min_files: usize,
    /// Token ceiling for one cluster context (supporting evidence is dropped first).
    pub max_tokens: usize,
    /// Lines of surrounding context around a primary symbol's span.
    pub primary_context_lines: usize,
    /// Upper bound on lines in any file-level code block.
    pub max_file_lines: usize,
    /// Tighter bound for supporting blocks.
    pub supporting_max_lines: usize,
    /// Cap on relationship candidate pairs in one pass.
    pub max_candidates: usize,
    /// Cap on candidates referencing any one entity.
    pub max_per_unit: usize,
    /// Cap on consolidation groups in one pass.
    pub max_groups: usize,
    /// Cap on symbols listed in one cluster context.
    pub max_nodes: usize,
}

impl Default for WikiSynthesisConfig {
    fn default() -> Self {
        Self {
            min_files: 1,
            max_tokens: 4000,
            primary_context_lines: 3,
            max_file_lines: 400,
            supporting_max_lines: 120,
            max_candidates: 60,
            max_per_unit: 6,
            max_groups: 40,
            max_nodes: 60,
        }
    }
}

/// Wiki indexing scope and write protection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WikiScopeConfig {
    /// Scaffold-relative globs never indexed.
    pub exclude: Vec<String>,
    /// Scaffold-relative globs operations never write (in addition to team-owned paths).
    pub read_only: Vec<String>,
    /// Extra entity types registered for this project.
    pub entity_types: Vec<String>,
    pub synthesis: WikiSynthesisConfig,
}

impl Default for WikiScopeConfig {
    fn default() -> Self {
        Self {
            exclude: WIKI_DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect(),
            read_only: Vec::new(),
            entity_types: Vec::new(),
            synthesis: WikiSynthesisConfig::default(),
        }
    }
}

impl WikiScopeConfig {
    /// Read the `wiki` section of `<scaffold_root>/config.json` leniently.
    pub fn load(scaffold_root: &Path) -> Self {
        let value = fs::read_to_string(scaffold_root.join("config.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok());
        match value.as_ref().and_then(|v| v.get("wiki")) {
            Some(wiki) => Self::from_json(wiki),
            None => Self::default(),
        }
    }

    pub fn from_json(wiki: &serde_json::Value) -> Self {
        fn globs(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
            let arr = v?.as_array()?;
            Some(
                arr.iter()
                    .filter_map(|e| e.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        }
        let mut out = Self::default();
        if let Some(ex) = globs(wiki.get("exclude")).filter(|g| !g.is_empty()) {
            out.exclude = ex;
        }
        // `readOnly` (globs, or `true` for the whole scaffold); legacy `read_only: true`.
        match wiki.get("readOnly").or_else(|| wiki.get("read_only")) {
            Some(serde_json::Value::Bool(true)) => out.read_only = vec!["**".to_string()],
            other => {
                if let Some(g) = globs(other) {
                    out.read_only = g;
                }
            }
        }
        if let Some(t) = globs(wiki.get("entityTypes")) {
            out.entity_types = t;
        }
        if let Some(s) = wiki.get("synthesis") {
            let mut syn = WikiSynthesisConfig::default();
            let set = |key: &str, slot: &mut usize| {
                if let Some(n) = s.get(key).and_then(|v| v.as_u64()).filter(|n| *n > 0) {
                    *slot = n as usize;
                }
            };
            set("minFiles", &mut syn.min_files);
            set("maxTokens", &mut syn.max_tokens);
            set("primaryContextLines", &mut syn.primary_context_lines);
            set("maxFileLines", &mut syn.max_file_lines);
            set("supportingMaxLines", &mut syn.supporting_max_lines);
            set("maxCandidates", &mut syn.max_candidates);
            set("maxPerUnit", &mut syn.max_per_unit);
            set("maxGroups", &mut syn.max_groups);
            set("maxNodes", &mut syn.max_nodes);
            out.synthesis = syn;
        }
        out
    }
}


// ---------------------------------------------------------------------------
// Code graph corpus policy (`graph` section of `.knobyte/config.json`)
// ---------------------------------------------------------------------------

/// `graph` section of `.knobyte/config.json`:
///
/// ```json
/// { "graph": { "ignore": ["generated/**", "**/*.pb.rs"], "max_file_bytes": 2097152, "max_files": 20000, "max_total_bytes": 536870912 } }
/// ```
///
/// `ignore` globs are additive: they can exclude more of the repository but never re-include the
/// built-in exclusions (`.git`, `node_modules`, `target`, `.knobyte`, ...).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GraphConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    /// Source files larger than this are skipped (reported, not fatal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_bytes: Option<u64>,
    /// Corpus walks finding more indexable files than this abort instead of indexing a partial
    /// repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<usize>,
    /// Corpus walks finding more indexable source bytes than this (default 512 MiB) abort
    /// instead of indexing a partial repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_bytes: Option<u64>,
}

/// Read the `graph` section of `<scaffold_root>/config.json`. A missing, unreadable or malformed
/// file (or section) yields `None`; this never fails a build.
pub fn read_graph_config_section(scaffold_root: &Path) -> Option<GraphConfig> {
    let path = scaffold_root.join("config.json");
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 256 * 1024 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    serde_json::from_value(value.get("graph")?.clone()).ok()
}

// ---------------------------------------------------------------------------
// Agent integration and maintenance keys (`aiTools`, `heartbeat`, `watch` in config.json).
//
// ```json
// { "aiTools": ["claude", "cursor"],
//   "heartbeat": { "staleDays": 7, "memoryCleanupDays": 7, "dailyMemoryRetentionDays": 14 },
//   "watch": { "intervalMinutes": 30 } }
// ```
//
// Each key is read and written on its own so unrelated sections are never rewritten.
// ---------------------------------------------------------------------------

/// The `scaffold_id` recorded in `config.json`, `None` when absent (unlike
/// [`KnobyteConfig::scaffold_id`], which falls back to a fresh random id).
pub fn persisted_scaffold_id(scaffold_root: &Path) -> Option<String> {
    read_config_value(scaffold_root)?
        .get("scaffold_id")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The whole `config.json` as JSON, `None` when absent or not a JSON object.
pub fn read_config_value(scaffold_root: &Path) -> Option<serde_json::Value> {
    let path = scaffold_root.join("config.json");
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    value.is_object().then_some(value)
}

/// Set one top-level key of `config.json`, preserving every other key. Creates the file (and
/// the scaffold directory) when missing; refuses to overwrite a file that is not a JSON object.
pub fn write_config_key(scaffold_root: &Path, key: &str, value: serde_json::Value) -> std::io::Result<()> {
    let path = scaffold_root.join("config.json");
    let mut root = match fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) if v.is_object() => v,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} is not a JSON object; fix it before rerunning", path.display()),
                ))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };
    root[key] = value;
    fs::create_dir_all(scaffold_root)?;
    fs::write(path, serde_json::to_string_pretty(&root)?)
}

/// AI tools setup may configure.
pub const AI_TOOLS: &[&str] = &["claude", "cursor", "windsurf", "copilot", "opencode", "codex"];

/// The saved `aiTools` selection (also read as `ai_tools`). `None` when no selection was ever
/// saved; `Some(vec![])` records an explicit decision to install no tool config. Unknown names
/// are dropped and duplicates removed.
pub fn load_ai_tools(scaffold_root: &Path) -> Option<Vec<String>> {
    let value = read_config_value(scaffold_root)?;
    let list = value
        .get("aiTools")
        .or_else(|| value.get("ai_tools"))?
        .as_array()?
        .clone();
    let mut out: Vec<String> = Vec::new();
    for v in list {
        if let Some(s) = v.as_str() {
            let s = s.trim().to_ascii_lowercase();
            if AI_TOOLS.contains(&s.as_str()) && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    Some(out)
}

/// Persist the `aiTools` selection.
pub fn save_ai_tools(scaffold_root: &Path, tools: &[String]) -> std::io::Result<()> {
    write_config_key(scaffold_root, "aiTools", serde_json::json!(tools))
}

/// `heartbeat` section of config.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatSettings {
    /// Days since a scaffold file's `last_updated` before it is reported stale (0 = older than today).
    pub stale_days: u64,
    /// Days since `memory/.last-cleanup.json` before a memory cleanup is due.
    pub memory_cleanup_days: u64,
    /// Daily memory files (`memory/YYYY-MM-DD.md`) older than this are retention candidates.
    pub daily_memory_retention_days: u64,
}

impl Default for HeartbeatSettings {
    fn default() -> Self {
        Self { stale_days: 7, memory_cleanup_days: 7, daily_memory_retention_days: 14 }
    }
}

fn non_negative(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0 && f.is_finite()).map(|f| f as u64))
}

impl HeartbeatSettings {
    /// Lenient read: malformed or negative values fall back to defaults.
    pub fn load(scaffold_root: &Path) -> Self {
        let mut out = Self::default();
        let Some(value) = read_config_value(scaffold_root) else {
            return out;
        };
        let Some(h) = value.get("heartbeat").filter(|h| h.is_object()) else {
            return out;
        };
        if let Some(n) = non_negative(h.get("staleDays").or_else(|| h.get("stale_days"))) {
            out.stale_days = n;
        }
        if let Some(n) =
            non_negative(h.get("memoryCleanupDays").or_else(|| h.get("memory_cleanup_days")))
        {
            out.memory_cleanup_days = n;
        }
        if let Some(n) = non_negative(
            h.get("dailyMemoryRetentionDays")
                .or_else(|| h.get("daily_memory_retention_days")),
        ) {
            out.daily_memory_retention_days = n;
        }
        out
    }

    /// Whether config.json sets `heartbeat.staleDays` explicitly.
    pub fn stale_days_configured(scaffold_root: &Path) -> bool {
        read_config_value(scaffold_root)
            .and_then(|v| v.get("heartbeat").cloned())
            .map(|h| h.get("staleDays").or_else(|| h.get("stale_days")).is_some())
            .unwrap_or(false)
    }
}

/// Default `knobyte watch --interval` period in minutes (`watch.intervalMinutes`, default 30).
pub fn watch_interval_minutes(scaffold_root: &Path) -> u64 {
    read_config_value(scaffold_root)
        .and_then(|v| {
            v.get("watch")
                .and_then(|w| w.get("intervalMinutes").or_else(|| w.get("interval_minutes")))
                .and_then(|n| n.as_u64())
        })
        .filter(|n| *n > 0)
        .unwrap_or(30)
}

// ---------------------------------------------------------------------------
// TypeScript type-checker mode (`graph.typescript` section of `.knobyte/config.json`)
// ---------------------------------------------------------------------------

/// `graph.typescript` section of `.knobyte/config.json`:
///
/// ```json
/// { "graph": { "typescript": { "compiler": "tsc", "typescript_path": "tools/node_modules/typescript",
///                              "node_path": "/usr/local/bin/node", "timeout_secs": 300 } } }
/// ```
///
/// `compiler` is `"source"` (the default: pure-Rust, source-only extraction) or `"tsc"` (opt-in:
/// resolve TS/JS calls, overloads, aliases and signatures with the TypeScript type checker when
/// Node and a `typescript` package are available; otherwise warn and fall back to source-only).
/// The project's own `node_modules/typescript` is used first, then `typescript_path` (when set,
/// it is authoritative), then a global install found read-only: `npm root -g`, the Node binary's
/// prefix `lib/node_modules`, Homebrew's `opt/typescript/libexec/lib/node_modules/typescript`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GraphTypeScriptConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiler: Option<String>,
    /// A `typescript` package directory (or its `lib/typescript.js`), relative to the project
    /// root or absolute. Used when the project has no `node_modules/typescript`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typescript_path: Option<String>,
    /// The Node executable (default: `node` on `PATH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_path: Option<String>,
    /// Upper bound for one type-checker run (default 300 seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl GraphTypeScriptConfig {
    /// True when the type-checker mode is requested (`compiler: "tsc"`).
    pub fn compiler_requested(&self) -> bool {
        self.compiler
            .as_deref()
            .is_some_and(|c| matches!(c.trim().to_ascii_lowercase().as_str(), "tsc" | "typescript" | "compiler"))
    }
}

/// Read `graph.typescript` from `<scaffold_root>/config.json`. Missing or malformed yields
/// `None`; this never fails a build.
pub fn read_graph_typescript_config(scaffold_root: &Path) -> Option<GraphTypeScriptConfig> {
    let value = read_config_value(scaffold_root)?;
    serde_json::from_value(value.get("graph")?.get("typescript")?.clone()).ok()
}
