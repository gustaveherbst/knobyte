//! Local Model2Vec static embedding backend and model management.
//!
//! Model2Vec models (e.g. `minishlab/potion-base-8M`) are static embeddings distilled from a
//! sentence transformer: inference is tokenize -> look up one row per token -> mean-pool ->
//! normalize, so it runs in pure Rust with no ONNX/libtorch runtime.
//!
//! Models are never downloaded implicitly: `knobyte cozo model pull` fetches `config.json`,
//! `tokenizer.json` and `model.safetensors` from Hugging Face into
//! `~/.knobyte/models/<owner>--<name>/` (override the root with `KNOBYTE_MODELS_DIR`).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use model2vec_rs::model::StaticModel;
use serde::{Deserialize, Serialize};

use super::embedding::{
    humanize_identifiers, normalize_or_fallback, unit_fallback, Embedder, HashedEmbedder,
    EMBEDDING_DIM, HASHED_EMBEDDER_ID,
};
use crate::config::{EmbeddingBackend, EmbeddingConfig};

/// Files that make up a Model2Vec model directory.
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// Max tokens per embedded field (Model2Vec default).
const MAX_TOKENS: usize = 512;

/// A loaded Model2Vec model.
pub struct Model2VecEmbedder {
    model: StaticModel,
    repo: String,
    dim: usize,
    path: PathBuf,
}

impl std::fmt::Debug for Model2VecEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model2VecEmbedder")
            .field("repo", &self.repo)
            .field("dim", &self.dim)
            .field("path", &self.path)
            .finish()
    }
}

impl Model2VecEmbedder {
    /// Load a model from a local directory containing [`MODEL_FILES`]. `repo` names the
    /// embedding space (it is part of [`Embedder::id`]).
    pub fn load(dir: &Path, repo: &str) -> Result<Self, String> {
        for f in MODEL_FILES {
            if !dir.join(f).is_file() {
                return Err(format!(
                    "Model2Vec model '{}' is incomplete: {} is missing in {}",
                    repo,
                    f,
                    dir.display()
                ));
            }
        }
        let model = StaticModel::from_pretrained(dir, None, None, None).map_err(|e| {
            format!(
                "Failed to load Model2Vec model '{}' from {}: {:#}",
                repo,
                dir.display(),
                e
            )
        })?;
        let dim = model.encode_single("dimension probe").len();
        if dim == 0 {
            return Err(format!(
                "Model2Vec model '{}' produced empty embeddings",
                repo
            ));
        }
        Ok(Self {
            model,
            repo: repo.to_string(),
            dim,
            path: dir.to_path_buf(),
        })
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Embedder for Model2VecEmbedder {
    fn id(&self) -> String {
        format!("model2vec:{}", self.repo)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_raw(text)
            .map(normalize_or_fallback)
            .unwrap_or_else(|| unit_fallback(self.dim))
    }

    /// Weighted sum of per-field embeddings; fields with no known tokens are skipped.
    fn embed_fields(&self, fields: &[(String, f32)]) -> Vec<f32> {
        let mut acc = vec![0.0f32; self.dim];
        let mut any = false;
        for (text, weight) in fields {
            if *weight <= 0.0 {
                continue;
            }
            if let Some(v) = self.embed_raw(text) {
                let v = normalize_or_fallback(v);
                any = true;
                for (a, x) in acc.iter_mut().zip(v) {
                    *a += x * weight;
                }
            }
        }
        if any {
            normalize_or_fallback(acc)
        } else {
            unit_fallback(self.dim)
        }
    }
}

impl Model2VecEmbedder {
    /// Mean-pooled embedding of `text` (identifiers split into words), or `None` when the text
    /// has no in-vocabulary tokens.
    fn embed_raw(&self, text: &str) -> Option<Vec<f32>> {
        let prepared = humanize_identifiers(text);
        if prepared.trim().is_empty() {
            return None;
        }
        let v = self
            .model
            .encode_with_args(&[prepared], Some(MAX_TOKENS), 1)
            .into_iter()
            .next()?;
        let norm_sq: f32 = v.iter().map(|x| x * x).sum();
        (v.len() == self.dim && norm_sq > 1e-12 && norm_sq.is_finite()).then_some(v)
    }
}

/// Root directory for downloaded models: `$KNOBYTE_MODELS_DIR`, else `~/.knobyte/models`.
pub fn models_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("KNOBYTE_MODELS_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("KNOBYTE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("models");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".knobyte").join("models")
}

/// Validate a Hugging Face repo id (`owner/name`, ASCII letters, digits, `-`, `_`, `.`).
pub fn validate_repo(repo: &str) -> Result<(), String> {
    let parts: Vec<&str> = repo.split('/').collect();
    let ok = parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "Invalid model id '{}': expected a Hugging Face repo like 'minishlab/potion-base-8M'",
            repo
        ))
    }
}

/// Local directory of a model: `<models_root>/<owner>--<name>`.
pub fn model_dir(repo: &str) -> PathBuf {
    models_root().join(repo.replace('/', "--"))
}

/// Whether all model files of `repo` are present locally.
pub fn model_present(repo: &str) -> bool {
    let dir = model_dir(repo);
    MODEL_FILES.iter().all(|f| dir.join(f).is_file())
}

/// Embedding dimension read from the safetensors header (no full model load).
pub fn read_model_dim(dir: &Path) -> Option<usize> {
    let mut f = fs::File::open(dir.join("model.safetensors")).ok()?;
    let mut len_buf = [0u8; 8];
    f.read_exact(&mut len_buf).ok()?;
    let len = u64::from_le_bytes(len_buf);
    if len == 0 || len > 100 * 1024 * 1024 {
        return None;
    }
    let mut header = vec![0u8; len as usize];
    f.read_exact(&mut header).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&header).ok()?;
    ["embeddings", "0", "embedding.weight"]
        .iter()
        .find_map(|k| v.get(*k))
        .and_then(|t| t.get("shape"))
        .and_then(|s| s.as_array())
        .filter(|s| s.len() == 2)
        .and_then(|s| s[1].as_u64())
        .map(|d| d as usize)
}

fn model_cache() -> &'static Mutex<HashMap<PathBuf, Arc<Model2VecEmbedder>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Model2VecEmbedder>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Error shown when the configured model has not been downloaded.
pub fn missing_model_error(repo: &str) -> String {
    format!(
        "Embedding backend 'model2vec' is configured but model '{}' is not downloaded (expected in {}). \
         Run 'knobyte cozo model pull --model {}' to download it, or switch back with \
         'knobyte cozo model use hashed'.",
        repo,
        model_dir(repo).display(),
        repo
    )
}

/// Build the embedder configured in `.knobyte/config.json`. Never downloads anything:
/// a configured but missing model is an error.
pub fn embedder_from_config(cfg: &EmbeddingConfig) -> Result<Arc<dyn Embedder>, String> {
    match cfg.backend {
        EmbeddingBackend::Hashed => Ok(Arc::new(HashedEmbedder)),
        EmbeddingBackend::Model2vec => {
            let repo = cfg.model_repo();
            validate_repo(&repo)?;
            if !model_present(&repo) {
                return Err(missing_model_error(&repo));
            }
            let dir = model_dir(&repo);
            let mut cache = model_cache().lock().unwrap_or_else(|p| p.into_inner());
            if let Some(m) = cache.get(&dir) {
                return Ok(m.clone() as Arc<dyn Embedder>);
            }
            let loaded = Arc::new(Model2VecEmbedder::load(&dir, &repo)?);
            cache.insert(dir, loaded.clone());
            Ok(loaded as Arc<dyn Embedder>)
        }
    }
}

/// Configured embedding backend and whether it is usable (no model load).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingStatus {
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    pub model_present: bool,
    /// Embedder id the vector indices are built with when this backend is active.
    pub embedder_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<usize>,
}

pub fn embedding_status(cfg: &EmbeddingConfig) -> EmbeddingStatus {
    match cfg.backend {
        EmbeddingBackend::Hashed => EmbeddingStatus {
            backend: "hashed".to_string(),
            model: None,
            model_path: None,
            model_present: true,
            embedder_id: HASHED_EMBEDDER_ID.to_string(),
            dim: Some(EMBEDDING_DIM),
        },
        EmbeddingBackend::Model2vec => {
            let repo = cfg.model_repo();
            let dir = model_dir(&repo);
            let present = validate_repo(&repo).is_ok() && model_present(&repo);
            EmbeddingStatus {
                backend: "model2vec".to_string(),
                model: Some(repo.clone()),
                model_path: Some(dir.display().to_string()),
                model_present: present,
                embedder_id: format!("model2vec:{}", repo),
                dim: if present { read_model_dim(&dir) } else { None },
            }
        }
    }
}

/// Result of `knobyte cozo model pull`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullReport {
    pub model: String,
    pub path: String,
    pub dim: usize,
    pub total_bytes: u64,
    pub downloaded: Vec<String>,
}

/// Download a Model2Vec model from Hugging Face into [`model_dir`]. Files already present are
/// kept unless `force`. Files are written to `*.part` and renamed once complete, then the
/// model is loaded once to verify it parses.
pub fn pull_model(repo: &str, force: bool, show_progress: bool) -> Result<PullReport, String> {
    validate_repo(repo)?;
    let dir = model_dir(repo);
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(20))
        .timeout_read(std::time::Duration::from_secs(60))
        .user_agent(concat!("knobyte/", env!("CARGO_PKG_VERSION")))
        .build();

    let mut downloaded = Vec::new();
    for file in MODEL_FILES {
        let dest = dir.join(file);
        if dest.is_file() && !force {
            continue;
        }
        let url = format!("https://huggingface.co/{}/resolve/main/{}", repo, file);
        download_file(&agent, &url, &dest, file, show_progress)?;
        downloaded.push(file.to_string());
    }

    // Verify: config parses, model loads, dimension matches the header.
    let cfg_text = fs::read_to_string(dir.join("config.json"))
        .map_err(|e| format!("Failed to read config.json: {}", e))?;
    serde_json::from_str::<serde_json::Value>(&cfg_text)
        .map_err(|e| format!("Downloaded config.json is not valid JSON: {}", e))?;
    let model = Model2VecEmbedder::load(&dir, repo)?;
    let total_bytes = MODEL_FILES
        .iter()
        .filter_map(|f| fs::metadata(dir.join(f)).ok())
        .map(|m| m.len())
        .sum();
    model_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&dir);

    Ok(PullReport {
        model: repo.to_string(),
        path: dir.display().to_string(),
        dim: model.dim(),
        total_bytes,
        downloaded,
    })
}

fn download_file(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    label: &str,
    show_progress: bool,
) -> Result<(), String> {
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(404, _) => {
            format!("{} not found at {} (is the model id right?)", label, url)
        }
        ureq::Error::Status(code, _) => format!("Download of {} failed: HTTP {}", url, code),
        other => format!("Download of {} failed: {}", url, other),
    })?;
    let total: Option<u64> = resp.header("Content-Length").and_then(|v| v.parse().ok());

    let bar = if show_progress {
        let bar = match total {
            Some(t) => indicatif::ProgressBar::new(t),
            None => indicatif::ProgressBar::new_spinner(),
        };
        if let Ok(style) = indicatif::ProgressStyle::with_template(
            "  {msg:<18} [{bar:30}] {bytes}/{total_bytes} ({bytes_per_sec})",
        ) {
            bar.set_style(style.progress_chars("=> "));
        }
        bar.set_message(label.to_string());
        Some(bar)
    } else {
        None
    };

    let part = dest.with_extension(format!(
        "{}.part",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let mut out = fs::File::create(&part)
        .map_err(|e| format!("Failed to create {}: {}", part.display(), e))?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("Download of {} interrupted: {}", url, e))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("Failed to write {}: {}", part.display(), e))?;
        written += n as u64;
        if let Some(b) = &bar {
            b.set_position(written);
        }
    }
    out.flush()
        .map_err(|e| format!("Failed to write {}: {}", part.display(), e))?;
    drop(out);
    if let Some(t) = total {
        if written != t {
            let _ = fs::remove_file(&part);
            return Err(format!(
                "Download of {} incomplete ({} of {} bytes)",
                url, written, t
            ));
        }
    }
    fs::rename(&part, dest)
        .map_err(|e| format!("Failed to move {} into place: {}", dest.display(), e))?;
    if let Some(b) = bar {
        b.finish_with_message(format!("{} done", label));
    }
    Ok(())
}
