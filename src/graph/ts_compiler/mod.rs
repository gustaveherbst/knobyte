//! Optional TypeScript type-checker mode for the code graph.
//!
//! Knobyte's TS/JS extraction is pure Rust and source-only by default. When a project opts in
//! (`graph.typescript.compiler: "tsc"` in `.knobyte/config.json`, or `--ts-compiler` on
//! `knobyte graph` / `graph refresh` / `graph rebuild`) and Node plus a `typescript` package are
//! available (the project's `node_modules/typescript` first, then `graph.typescript.typescript_path`,
//! then read-only discovery of a global install: `npm root -g`, the Node binary's prefix
//! `lib/node_modules/typescript`, and Homebrew's `opt/typescript/libexec/lib/node_modules/typescript`
//! under `brew --prefix`, `/opt/homebrew` and `/usr/local`),
//! the build runs a helper script embedded in the binary (`helper.js`, written to
//! `.knobyte/ts-compiler/`) that loads the TypeScript compiler API, builds one program per
//! tsconfig project and prints JSON facts: checker-resolved call targets (typed receivers,
//! `new C().m()`, `super.m()`, overloads, aliased imports), checker-rendered signatures and
//! resolved type aliases. The build merges them as high-confidence edges with provenance
//! `typescript-compiler`; everything else stays source-derived.
//!
//! Nothing is ever installed or downloaded, and the helper does no network I/O. When Node or the
//! `typescript` package is missing, or the helper fails or times out, the build warns once and
//! continues source-only: this mode never fails a build.
//!
//! Helper results are cached per file in `.knobyte/ts-compiler/facts-cache.json`, keyed by the
//! file's content hash under a digest of the helper, the TypeScript version and every tsconfig /
//! jsconfig / package manifest / lockfile of the project. A changed file is re-checked together
//! with the transitive reverse-import closure of the files that (by the checker's own module
//! resolution) depend on it; a change the closure cannot see (an added or deleted file, a changed
//! script or declaration file visible without an import, any configuration byte) re-checks the
//! whole corpus.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{read_graph_typescript_config, GraphTypeScriptConfig, DEFAULT_SCAFFOLD_DIR};
use crate::graph::models::ExtractedCall;
use crate::graph::resolve::NodeMeta;

/// Provenance and resolution method of every edge derived from the type checker.
pub const PROVENANCE: &str = "typescript-compiler";

const HELPER_SOURCE: &str = include_str!("helper.js");
/// Output format the embedded helper prints (`version` in its JSON).
const HELPER_FORMAT: u32 = 1;
const CACHE_FORMAT: u32 = 1;
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const CACHE_DIR: &str = "ts-compiler";
const CACHE_FILE: &str = "facts-cache.json";
/// Directories never searched for project configuration (same spirit as the helper's list).
const IGNORED_DIRECTORIES: [&str; 15] = [
    ".git", ".hg", ".svn", ".knobyte", ".next", ".nuxt", ".turbo", ".cache", "build", "coverage",
    "dist", "node_modules", "out", "target", "vendor",
];

static CLI_REQUEST: AtomicBool = AtomicBool::new(false);
static WARNED: AtomicBool = AtomicBool::new(false);

/// Request the type-checker mode for every graph build of this process (`--ts-compiler`).
pub fn request_for_process() {
    CLI_REQUEST.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Helper facts
// ---------------------------------------------------------------------------

/// A declaration the checker resolved to: file, line of its name, and name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Location {
    pub file: String,
    pub line: usize,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CallFact {
    pub line: usize,
    /// UTF-8 byte column of the call expression's start (tree-sitter's column convention).
    pub col: usize,
    pub name: String,
    pub targets: Vec<Location>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DeclFact {
    pub line: usize,
    pub name: String,
    pub signature: String,
    #[serde(default, rename = "returnType")]
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AliasFact {
    pub line: usize,
    pub name: String,
    pub target: Location,
}

/// Everything the checker reported about one file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct FileFacts {
    /// In-project files this file's module specifiers resolve to.
    #[serde(default)]
    pub deps: Vec<String>,
    /// A script or declaration file (or one with global / module augmentations): its
    /// declarations are visible without an import.
    #[serde(default)]
    pub global: bool,
    #[serde(default)]
    pub calls: Vec<CallFact>,
    #[serde(default)]
    pub decls: Vec<DeclFact>,
    #[serde(default)]
    pub aliases: Vec<AliasFact>,
}

#[derive(Debug, Deserialize)]
struct HelperOutput {
    version: u32,
    #[serde(default)]
    files: HashMap<String, FileFacts>,
    #[serde(default)]
    warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Settings and toolchain discovery
// ---------------------------------------------------------------------------

struct Settings {
    requested: bool,
    config: GraphTypeScriptConfig,
    timeout: Duration,
}

fn settings(root: &Path) -> Settings {
    let config = read_graph_typescript_config(&root.join(DEFAULT_SCAFFOLD_DIR)).unwrap_or_default();
    let requested = CLI_REQUEST.load(Ordering::SeqCst) || config.compiler_requested();
    let timeout = Duration::from_secs(config.timeout_secs.filter(|s| *s > 0).unwrap_or(DEFAULT_TIMEOUT_SECS));
    Settings {
        requested,
        config,
        timeout,
    }
}

struct Toolchain {
    node: PathBuf,
    /// What the helper `require`s: a `typescript` package directory or its main file.
    typescript: PathBuf,
    version: String,
    /// Where the package was found (`project node_modules`, `graph.typescript.typescript_path`,
    /// `npm root -g`, `node prefix`, `homebrew`).
    source: &'static str,
}

/// Timeout of the read-only `npm root -g` / `brew --prefix` probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A `typescript` package at `path` (a package directory, or a file inside one): its require
/// path and version.
fn typescript_package(path: &Path) -> Option<(PathBuf, String)> {
    let (dir, entry) = if path.is_file() {
        let mut dir = path.parent()?;
        // `.../typescript/lib/typescript.js` -> `.../typescript`
        if dir.file_name().is_some_and(|n| n == "lib") {
            dir = dir.parent()?;
        }
        (dir.to_path_buf(), path.to_path_buf())
    } else {
        (path.to_path_buf(), path.to_path_buf())
    };
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
    if manifest.get("name").and_then(|n| n.as_str()) != Some("typescript") {
        return None;
    }
    let version = manifest.get("version")?.as_str()?.to_string();
    if entry.is_dir() && !dir.join("lib").join("typescript.js").is_file() {
        return None;
    }
    Some((entry.canonicalize().unwrap_or(entry), version))
}

type Found = (PathBuf, String, &'static str);

/// The project's `node_modules/typescript`, then `graph.typescript.typescript_path` (an explicit
/// path is authoritative: when it does not name a package, nothing else is tried), then a
/// globally installed package (read-only discovery; nothing is installed).
fn find_typescript(root: &Path, config: &GraphTypeScriptConfig, node: Option<&Path>) -> Option<Found> {
    if let Some((p, v)) = typescript_package(&root.join("node_modules").join("typescript")) {
        return Some((p, v, "project node_modules"));
    }
    if let Some(configured) = config.typescript_path.as_deref().filter(|p| !p.trim().is_empty()) {
        let p = PathBuf::from(configured);
        let p = if p.is_absolute() { p } else { root.join(p) };
        return typescript_package(&p)
            .or_else(|| typescript_package(&p.join("node_modules").join("typescript")))
            .map(|(p, v)| (p, v, "graph.typescript.typescript_path"));
    }
    global_typescript(node)
}

/// A globally installed `typescript` package, discovered once per process.
fn global_typescript(node: Option<&Path>) -> Option<Found> {
    static FOUND: std::sync::OnceLock<Option<Found>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            global_typescript_candidates(node)
                .into_iter()
                .find_map(|(dir, source)| typescript_package(&dir).map(|(p, v)| (p, v, source)))
        })
        .clone()
}

/// Candidate global package directories, in order: `npm root -g`, the Node binary's prefix,
/// Homebrew (`brew --prefix`, then `/opt/homebrew` and `/usr/local`).
fn global_typescript_candidates(node: Option<&Path>) -> Vec<(PathBuf, &'static str)> {
    let mut out = Vec::new();
    let npm_names: &[&str] = if cfg!(windows) { &["npm.cmd", "npm.exe", "npm"] } else { &["npm"] };
    if let Some(npm) = find_on_path(npm_names) {
        if let Some(root) = probe_output(&npm, &["root", "-g"]) {
            out.push((PathBuf::from(root).join("typescript"), "npm root -g"));
        }
    }
    if let Some(node) = node {
        let node = node.canonicalize().unwrap_or_else(|_| node.to_path_buf());
        if let Some(prefix) = node.parent().and_then(|bin| bin.parent()) {
            out.push((prefix.join("lib/node_modules/typescript"), "node prefix"));
            out.push((prefix.join("node_modules/typescript"), "node prefix"));
        }
    }
    let brew_formula = |prefix: &Path| prefix.join("opt/typescript/libexec/lib/node_modules/typescript");
    if let Some(brew) = find_on_path(&["brew"]) {
        if let Some(prefix) = probe_output(&brew, &["--prefix"]) {
            out.push((brew_formula(Path::new(&prefix)), "homebrew"));
        }
    }
    if !cfg!(windows) {
        for prefix in ["/opt/homebrew", "/usr/local"] {
            out.push((brew_formula(Path::new(prefix)), "homebrew"));
            out.push((Path::new(prefix).join("lib/node_modules/typescript"), "homebrew"));
        }
    }
    out
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|c| c.is_file())
}

/// First stdout line of a short read-only probe (`npm root -g`, `brew --prefix`), or `None`
/// when it fails or exceeds [`PROBE_TIMEOUT`].
fn probe_output(program: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(64 * 1024).read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if started.elapsed() > PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let out = reader.join().ok()?;
    let line = String::from_utf8_lossy(&out).lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

fn find_node(config: &GraphTypeScriptConfig) -> Option<PathBuf> {
    if let Some(p) = config.node_path.as_deref().filter(|p| !p.trim().is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    find_on_path(if cfg!(windows) { &["node.exe", "node.cmd"] } else { &["node"] })
}

fn toolchain(root: &Path, settings: &Settings) -> Result<Toolchain, String> {
    let node = find_node(&settings.config);
    let (typescript, version, source) = find_typescript(root, &settings.config, node.as_deref()).ok_or_else(|| {
        if settings.config.typescript_path.as_deref().is_some_and(|p| !p.trim().is_empty()) {
            "graph.typescript.typescript_path does not name a `typescript` package".to_string()
        } else {
            "no `typescript` package in node_modules/typescript, graph.typescript.typescript_path, \
             `npm root -g`, the Node prefix or Homebrew"
                .to_string()
        }
    })?;
    let node =
        node.ok_or_else(|| "Node.js (`node`) was not found on PATH or graph.typescript.node_path".to_string())?;
    Ok(Toolchain {
        node,
        typescript,
        version,
        source,
    })
}

impl Toolchain {
    /// `<source>: <path>` of the package in use (reported in the build summary and status).
    fn location(&self) -> String {
        format!("{}: {}", self.source, self.typescript.display())
    }
}

/// Configuration inputs that change what the checker resolves: tsconfig / jsconfig files,
/// package manifests and lockfiles (proxies for the installed dependencies), sorted by path.
fn config_inputs(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn is_input(name: &str) -> bool {
        (name.starts_with("tsconfig") && name.ends_with(".json"))
            || matches!(
                name,
                "jsconfig.json" | "package.json" | "package-lock.json" | "npm-shrinkwrap.json"
                    | "pnpm-lock.yaml" | "yarn.lock" | "bun.lockb" | "bun.lock"
            )
    }
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            visited += 1;
            if visited > 500_000 {
                return out;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 24 && !IGNORED_DIRECTORIES.contains(&name.as_str()) {
                    stack.push((e.path(), depth + 1));
                }
            } else if ft.is_file() && is_input(&name) {
                if let Ok(bytes) = std::fs::read(e.path()) {
                    let rel = e.path().strip_prefix(root).unwrap_or(&e.path()).to_string_lossy().replace('\\', "/");
                    out.push((rel, bytes));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    hex::encode(h.finalize())
}

fn digest(root: &Path, tc: &Toolchain) -> String {
    let mut h = Sha256::new();
    h.update(format!("knobyte-ts-compiler cache {} helper {}\n", CACHE_FORMAT, HELPER_FORMAT));
    h.update(sha256_hex(&[HELPER_SOURCE.as_bytes()]));
    h.update(format!("\ntypescript {} at {}\n", tc.version, tc.typescript.display()));
    for (path, bytes) in config_inputs(root) {
        h.update(format!("{}\0{}\n", path, sha256_hex(&[&bytes])));
    }
    hex::encode(h.finalize())
}

/// The mode a graph is built under, recorded in the graph's metadata so that switching the mode
/// (or changing the checker's configuration inputs) makes the next refresh re-resolve:
/// `source`, `tsc:<digest>` or `tsc:unavailable`.
pub(crate) fn mode_key(root: &Path) -> String {
    let s = settings(root);
    if !s.requested {
        return "source".to_string();
    }
    match toolchain(root, &s) {
        Ok(tc) => format!("tsc:{}", digest(root, &tc)),
        Err(_) => "tsc:unavailable".to_string(),
    }
}

fn warn_once(reason: &str) {
    if !WARNED.swap(true, Ordering::SeqCst) {
        eprintln!(
            "knobyte: warning: TypeScript compiler mode unavailable ({}); using source-only TS/JS extraction",
            reason
        );
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    format: u32,
    digest: String,
    files: BTreeMap<String, CachedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEntry {
    hash: String,
    facts: FileFacts,
}

fn cache_dir(root: &Path) -> Option<PathBuf> {
    let dir = root.join(DEFAULT_SCAFFOLD_DIR).join(CACHE_DIR);
    std::fs::create_dir_all(&dir).ok()?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "# Knobyte TypeScript checker helper and cache\n*\n");
    }
    Some(dir)
}

fn load_cache(dir: Option<&Path>, digest: &str) -> CacheFile {
    dir.and_then(|d| std::fs::read(d.join(CACHE_FILE)).ok())
        .and_then(|b| serde_json::from_slice::<CacheFile>(&b).ok())
        .filter(|c| c.format == CACHE_FORMAT && c.digest == digest)
        .unwrap_or_else(|| CacheFile {
            format: CACHE_FORMAT,
            digest: digest.to_string(),
            files: BTreeMap::new(),
        })
}

fn save_cache(dir: Option<&Path>, cache: &CacheFile) {
    let Some(dir) = dir else { return };
    let Ok(bytes) = serde_json::to_vec(cache) else { return };
    let tmp = dir.join(format!("{}.tmp-{}", CACHE_FILE, std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, dir.join(CACHE_FILE)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Files whose facts must be recomputed: `None` means the whole corpus.
fn dirty_files(cache: &CacheFile, current: &BTreeMap<String, String>) -> Option<BTreeSet<String>> {
    if cache.files.is_empty() || !cache.files.keys().eq(current.keys()) {
        return None;
    }
    let changed: Vec<&String> = current
        .iter()
        .filter(|(f, h)| cache.files.get(*f).is_none_or(|e| &e.hash != *h))
        .map(|(f, _)| f)
        .collect();
    if changed.iter().any(|f| cache.files.get(*f).is_some_and(|e| e.facts.global)) {
        return None;
    }
    // Transitive reverse-import closure of the changed files.
    let mut importers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (f, e) in &cache.files {
        for d in &e.facts.deps {
            importers.entry(d.as_str()).or_default().push(f.as_str());
        }
    }
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<&str> = changed.iter().map(|f| f.as_str()).collect();
    while let Some(f) = queue.pop_front() {
        if !dirty.insert(f.to_string()) {
            continue;
        }
        for i in importers.get(f).map(|v| v.as_slice()).unwrap_or(&[]) {
            if !dirty.contains(*i) {
                queue.push_back(i);
            }
        }
    }
    Some(dirty)
}

// ---------------------------------------------------------------------------
// Running the helper
// ---------------------------------------------------------------------------

fn helper_path(dir: Option<&Path>) -> Result<PathBuf, String> {
    let name = format!("helper-{}.cjs", &sha256_hex(&[HELPER_SOURCE.as_bytes()])[..16]);
    let base = dir.map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
    let path = base.join(name);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(HELPER_SOURCE) {
        std::fs::write(&path, HELPER_SOURCE).map_err(|e| format!("cannot write the checker helper: {}", e))?;
    }
    Ok(path)
}

fn run_helper(
    root: &Path,
    tc: &Toolchain,
    helper: &Path,
    requested: &[String],
    candidates: &[String],
    timeout: Duration,
) -> Result<HelperOutput, String> {
    let input = serde_json::json!({
        "root": root.to_string_lossy(),
        "typescript": tc.typescript.to_string_lossy(),
        "requested": requested,
        "candidates": candidates,
    })
    .to_string();
    let mut child = Command::new(&tc.node)
        .arg(helper)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {}", tc.node.display(), e))?;
    let mut stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(s) = stdin.as_mut() {
            let _ = s.write_all(input.as_bytes());
        }
    });
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let mut stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.take(64 * 1024).read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the checker helper timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("the checker helper failed: {}", e)),
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("no output");
        return Err(format!("the checker helper exited with {} ({})", status, first.trim()));
    }
    let parsed: HelperOutput =
        serde_json::from_slice(&out).map_err(|e| format!("unreadable checker helper output: {}", e))?;
    if parsed.version != HELPER_FORMAT {
        return Err(format!("unexpected checker helper format {}", parsed.version));
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Build entry point
// ---------------------------------------------------------------------------

/// Checker facts for the TS/JS files of one build.
#[derive(Debug, Default)]
pub(crate) struct CompilerFacts {
    files: BTreeMap<String, FileFacts>,
}

pub(crate) struct Outcome {
    pub facts: Option<CompilerFacts>,
    /// Human-readable mode status (`source`, `typescript 5.9.3 (...)`, `source (fallback: ..)`).
    pub status: String,
    /// See [`mode_key`].
    pub mode_key: String,
}

/// Checker facts for `inputs` (TS/JS corpus files with their content hashes), from the cache
/// and, for files the cache cannot answer, from one helper run. `facts` is `None` in the default
/// source-only mode and whenever the checker is unavailable or fails (after one warning).
pub(crate) fn collect(root: &Path, inputs: &[(String, String)]) -> Outcome {
    let s = settings(root);
    if !s.requested {
        return Outcome {
            facts: None,
            status: "source".to_string(),
            mode_key: "source".to_string(),
        };
    }
    let fallback = |reason: String, key: String| {
        warn_once(&reason);
        Outcome {
            facts: None,
            status: format!("source (fallback: {})", reason),
            mode_key: key,
        }
    };
    let tc = match toolchain(root, &s) {
        Ok(tc) => tc,
        Err(reason) => return fallback(reason, "tsc:unavailable".to_string()),
    };
    let digest = digest(root, &tc);
    let mode_key = format!("tsc:{}", digest);
    if inputs.is_empty() {
        return Outcome {
            facts: Some(CompilerFacts::default()),
            status: format!("typescript {} from {} (no TS/JS files)", tc.version, tc.location()),
            mode_key,
        };
    }
    let dir = cache_dir(root);
    let mut cache = load_cache(dir.as_deref(), &digest);
    let current: BTreeMap<String, String> = inputs.iter().cloned().collect();
    let candidates: Vec<String> = current.keys().cloned().collect();

    let mut dirty = dirty_files(&cache, &current);
    let mut checked = 0usize;
    loop {
        let full = dirty.is_none();
        let requested: Vec<String> = match &dirty {
            None => candidates.clone(),
            Some(d) => d.iter().cloned().collect(),
        };
        if requested.is_empty() {
            break;
        }
        let helper = match helper_path(dir.as_deref()) {
            Ok(h) => h,
            Err(reason) => return fallback(reason, mode_key),
        };
        let mut out = match run_helper(root, &tc, &helper, &requested, &candidates, s.timeout) {
            Ok(o) => o,
            Err(reason) => return fallback(reason, mode_key),
        };
        let _ = &out.warnings;
        if full {
            cache.files.clear();
        }
        let mut became_global = false;
        for f in &requested {
            let facts = out.files.remove(f).unwrap_or_default();
            became_global |= facts.global;
            cache.files.insert(
                f.clone(),
                CachedEntry {
                    hash: current[f].clone(),
                    facts,
                },
            );
        }
        checked = requested.len();
        // A changed file that is now visible without an import affects files outside the
        // closure: check the whole corpus.
        if !full && became_global {
            dirty = None;
            continue;
        }
        break;
    }
    save_cache(dir.as_deref(), &cache);
    let files = cache
        .files
        .into_iter()
        .filter(|(f, _)| current.contains_key(f))
        .map(|(f, e)| (f, e.facts))
        .collect();
    Outcome {
        facts: Some(CompilerFacts { files }),
        status: format!(
            "typescript {} from {} ({} files checked, {} reused from cache)",
            tc.version,
            tc.location(),
            checked,
            current.len() - checked
        ),
        mode_key,
    }
}

// ---------------------------------------------------------------------------
// Merging facts into the graph
// ---------------------------------------------------------------------------

/// Checker facts mapped onto the graph's declarations.
pub(crate) struct Applied {
    /// (file, line, col, callee name) -> resolved targets, one entry per call in source order.
    calls: HashMap<(String, usize, usize, String), VecDeque<Option<usize>>>,
    /// (node index, signature, return type) for declarations the checker rendered.
    pub signatures: Vec<(usize, String, Option<String>)>,
    /// (type alias node, target node, line) pairs.
    pub aliases: Vec<(usize, usize, usize)>,
}

/// Index of the graph declaration a checker location names: same file and name, the innermost
/// declaration whose span contains the name's line.
fn locate(by_file_name: &HashMap<(&str, &str), Vec<usize>>, metas: &[NodeMeta], loc: &Location) -> Option<usize> {
    let cands = by_file_name.get(&(loc.file.as_str(), loc.name.as_str()))?;
    cands
        .iter()
        .copied()
        .filter(|&i| metas[i].start_line <= loc.line && loc.line <= metas[i].end_line)
        .min_by_key(|&i| (metas[i].end_line - metas[i].start_line, (metas[i].start_line != loc.line) as u8))
}

impl CompilerFacts {
    pub fn apply(&self, metas: &[NodeMeta]) -> Applied {
        let mut by_file_name: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, m) in metas.iter().enumerate() {
            if m.kind != "file" && m.kind != "module" {
                by_file_name.entry((m.file_path.as_str(), m.name.as_str())).or_default().push(i);
            }
        }
        let mut applied = Applied {
            calls: HashMap::new(),
            signatures: Vec::new(),
            aliases: Vec::new(),
        };
        for (file, facts) in &self.files {
            for c in &facts.calls {
                let mut targets: Vec<usize> = c
                    .targets
                    .iter()
                    .filter_map(|t| locate(&by_file_name, metas, t))
                    .collect();
                targets.sort_unstable();
                targets.dedup();
                // Only an answer naming exactly one graph declaration is used; anything else
                // (no in-project target, a union of several) leaves the call to the source
                // resolver.
                let target = (targets.len() == 1 && targets.len() == c.targets.len()).then(|| targets[0]);
                applied
                    .calls
                    .entry((file.clone(), c.line, c.col, c.name.clone()))
                    .or_default()
                    .push_back(target);
            }
            for d in &facts.decls {
                let loc = Location {
                    file: file.clone(),
                    line: d.line,
                    name: d.name.clone(),
                };
                if let Some(i) = locate(&by_file_name, metas, &loc)
                    .filter(|&i| matches!(metas[i].kind.as_str(), "function" | "method"))
                {
                    applied.signatures.push((i, d.signature.clone(), d.return_type.clone()));
                }
            }
            for a in &facts.aliases {
                let loc = Location {
                    file: file.clone(),
                    line: a.line,
                    name: a.name.clone(),
                };
                let from = locate(&by_file_name, metas, &loc).filter(|&i| metas[i].kind == "type_alias");
                let to = locate(&by_file_name, metas, &a.target);
                if let (Some(from), Some(to)) = (from, to) {
                    if from != to {
                        applied.aliases.push((from, to, a.line));
                    }
                }
            }
        }
        applied
    }
}

impl Applied {
    /// The declaration the checker resolved a source call to, if it named exactly one.
    pub fn call_target(&mut self, file: &str, call: &ExtractedCall) -> Option<usize> {
        let key = (file.to_string(), call.line, call.col, call.target_name.clone());
        self.calls.get_mut(&key)?.pop_front().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash: &str, deps: &[&str], global: bool) -> CachedEntry {
        CachedEntry {
            hash: hash.to_string(),
            facts: FileFacts {
                deps: deps.iter().map(|d| d.to_string()).collect(),
                global,
                ..Default::default()
            },
        }
    }

    #[test]
    fn dirty_set_is_the_reverse_import_closure() {
        let mut cache = CacheFile::default();
        cache.files.insert("a.ts".into(), entry("1", &[], false));
        cache.files.insert("b.ts".into(), entry("1", &["a.ts"], false));
        cache.files.insert("c.ts".into(), entry("1", &["b.ts"], false));
        cache.files.insert("d.ts".into(), entry("1", &[], false));
        cache.files.insert("g.d.ts".into(), entry("1", &[], true));
        let mut current: BTreeMap<String, String> =
            cache.files.iter().map(|(f, e)| (f.clone(), e.hash.clone())).collect();
        assert_eq!(dirty_files(&cache, &current), Some(BTreeSet::new()));
        current.insert("a.ts".into(), "2".into());
        let dirty = dirty_files(&cache, &current).unwrap();
        assert_eq!(dirty.into_iter().collect::<Vec<_>>(), vec!["a.ts", "b.ts", "c.ts"]);
        // A changed global file, or an added file, re-checks everything.
        current.insert("g.d.ts".into(), "2".into());
        assert_eq!(dirty_files(&cache, &current), None);
        current.insert("g.d.ts".into(), "1".into());
        current.insert("e.ts".into(), "1".into());
        assert_eq!(dirty_files(&cache, &current), None);
    }

    #[test]
    fn global_discovery_candidates_cover_node_prefix_and_homebrew() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("bin/node");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, "").unwrap();
        let cands = global_typescript_candidates(Some(&node));
        let prefix = dir.path().canonicalize().unwrap();
        assert!(cands.iter().any(|(p, s)| *s == "node prefix" && *p == prefix.join("lib/node_modules/typescript")));
        if !cfg!(windows) {
            for brew in ["/opt/homebrew", "/usr/local"] {
                let formula = Path::new(brew).join("opt/typescript/libexec/lib/node_modules/typescript");
                assert!(cands.iter().any(|(p, s)| *s == "homebrew" && *p == formula), "{:?}", cands);
            }
        }
        // A configured path is authoritative: no discovery behind it.
        let config = GraphTypeScriptConfig { typescript_path: Some("does/not/exist".into()), ..Default::default() };
        assert!(find_typescript(dir.path(), &config, Some(&node)).is_none());
    }

    #[test]
    fn typescript_package_requires_a_real_package() {
        let dir = tempfile::tempdir().unwrap();
        assert!(typescript_package(dir.path()).is_none());
        std::fs::write(dir.path().join("package.json"), r#"{"name":"typescript","version":"5.0.0"}"#).unwrap();
        assert!(typescript_package(dir.path()).is_none(), "no lib/typescript.js");
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::write(dir.path().join("lib/typescript.js"), "").unwrap();
        let (_, version) = typescript_package(dir.path()).unwrap();
        assert_eq!(version, "5.0.0");
        let (_, version) = typescript_package(&dir.path().join("lib/typescript.js")).unwrap();
        assert_eq!(version, "5.0.0");
    }
}
