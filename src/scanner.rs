//! Pre-analysis scanner (`knobyte init`): a compact brief of the repository (manifest, entry
//! points, top-level folders, tooling, README) so a populating agent can reason from facts
//! instead of exploring the filesystem.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::drift::checkers::{glob_regex, walk_index};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestInfo {
    /// `package.json`, `Cargo.toml`, `pyproject.toml`, `go.mod`, `Package.swift` or
    /// `xcodeproj` (an Xcode project/workspace with no SwiftPM manifest).
    #[serde(rename = "type")]
    pub kind: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub dependencies: BTreeMap<String, String>,
    pub dev_dependencies: BTreeMap<String, String>,
    pub scripts: BTreeMap<String, String>,
    /// Declared products (SwiftPM `products:`); empty for other manifests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub products: Vec<ManifestProduct>,
    /// Declared build targets (SwiftPM `targets:`); empty for other manifests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<ManifestTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestProduct {
    pub name: String,
    /// `library`, `executable` or `plugin`.
    #[serde(rename = "type")]
    pub kind: String,
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestTarget {
    pub name: String,
    /// `target`, `executableTarget`, `testTarget`, `macro`, `plugin`, `systemLibrary` or
    /// `binaryTarget`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Source directory (explicit `path:` or the SwiftPM default `Sources/<name>`,
    /// `Tests/<name>`, `Plugins/<name>`).
    pub path: String,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntryPoint {
    pub path: String,
    /// `main`, `test` or `config`.
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FolderCategory {
    pub name: String,
    pub path: String,
    pub file_count: usize,
    /// routes, models, services, tests, config, utils, views or other.
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolingInfo {
    pub test_runner: Option<String>,
    pub build_tool: Option<String>,
    pub linter: Option<String>,
    pub formatter: Option<String>,
    pub package_manager: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScannerBrief {
    /// The first manifest found, in the order package.json, pyproject.toml, go.mod, Cargo.toml,
    /// Package.swift, then an Xcode project.
    pub manifest: Option<ManifestInfo>,
    /// Every manifest present at the root (polyglot repositories).
    #[serde(default)]
    pub manifests: Vec<ManifestInfo>,
    pub entry_points: Vec<EntryPoint>,
    pub folder_tree: Vec<FolderCategory>,
    pub tooling: ToolingInfo,
    /// README content, truncated to 3000 characters.
    pub readme: Option<String>,
    pub timestamp: String,
}

const IGNORE_DIRS: &[&str] = &[
    "node_modules", ".git", "dist", "build", ".next", ".nuxt", "__pycache__", ".venv", "venv",
    "target", "vendor", ".knobyte", ".build", "DerivedData", ".swiftpm",
];

/// Build the brief for `project_root`.
pub fn scan(project_root: &Path) -> ScannerBrief {
    let manifests = scan_manifests(project_root);
    ScannerBrief {
        manifest: manifests.first().cloned(),
        manifests,
        entry_points: scan_entry_points(project_root),
        folder_tree: scan_folder_tree(project_root),
        tooling: scan_tooling(project_root),
        readme: scan_readme(project_root),
        timestamp: chrono::Utc::now().to_rfc3339(),
    }
}

/// The human prompt embedding `brief` (what `knobyte init` prints without `--json`).
pub fn build_prompt(brief: &ScannerBrief) -> String {
    let json = serde_json::to_string_pretty(brief).unwrap_or_default();
    format!(
        "Here is a pre-analyzed brief of the codebase. Do NOT explore the filesystem yourself; reason from this brief:\n\n<brief>\n{}\n</brief>\n\nUsing this brief, populate the Knobyte scaffold files. Focus on:\n1. .knobyte/context/architecture.md: system components, data flow, integrations\n2. .knobyte/context/stack.md: technologies, versions, key libraries\n3. .knobyte/context/conventions.md: code patterns, naming, file organization\n4. .knobyte/context/decisions.md: architectural choices and their rationale\n5. .knobyte/context/setup.md: how to set up and run the project\n6. .knobyte/ROUTER.md: update the \"Current Project State\" section\n\nFor each file, use the information from the brief rather than exploring the filesystem.\nBe precise about versions, paths, and dependencies: they come directly from the manifest.",
        json
    )
}

// ---------------------------------------------------------------------------- manifests

fn scan_manifests(root: &Path) -> Vec<ManifestInfo> {
    let mut out = Vec::new();
    for file in ["package.json", "pyproject.toml", "go.mod", "Cargo.toml", "Package.swift"] {
        let Ok(content) = fs::read_to_string(root.join(file)) else {
            continue;
        };
        let parsed = match file {
            "package.json" => parse_package_json(&content),
            "pyproject.toml" => Some(parse_pyproject(&content)),
            "go.mod" => Some(parse_go_mod(&content)),
            "Package.swift" => Some(parse_package_swift(&content)),
            _ => Some(parse_cargo(&content)),
        };
        if let Some(m) = parsed {
            out.push(m);
        }
    }
    if !root.join("Package.swift").is_file() {
        if let Some(name) = xcode_project_name(root) {
            let mut m = empty_manifest("xcodeproj");
            m.name = Some(name);
            out.push(m);
        }
    }
    out
}

/// The stem of the first `*.xcworkspace` / `*.xcodeproj` at the root (workspaces win).
pub fn xcode_project_name(root: &Path) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".xcworkspace") || n.ends_with(".xcodeproj"))
        .collect();
    names.sort_by_key(|n| (!n.ends_with(".xcworkspace"), n.clone()));
    names.first().map(|n| n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n).to_string())
}

fn empty_manifest(kind: &str) -> ManifestInfo {
    ManifestInfo {
        kind: kind.to_string(),
        name: None,
        version: None,
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
        scripts: BTreeMap::new(),
        products: Vec::new(),
        targets: Vec::new(),
    }
}

fn string_map(v: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    v.and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_package_json(content: &str) -> Option<ManifestInfo> {
    let raw: serde_json::Value = serde_json::from_str(content).ok()?;
    let mut m = empty_manifest("package.json");
    m.name = raw.get("name").and_then(|v| v.as_str()).map(str::to_string);
    m.version = raw.get("version").and_then(|v| v.as_str()).map(str::to_string);
    m.dependencies = string_map(raw.get("dependencies"));
    m.dev_dependencies = string_map(raw.get("devDependencies"));
    m.scripts = string_map(raw.get("scripts"));
    Some(m)
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

/// Minimal TOML section reader: `(section, key, raw value)` for single-line `key = value` pairs.
fn toml_pairs(content: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') {
            section = t.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            out.push((section.clone(), unquote(k), v.trim().to_string()));
        }
    }
    out
}

fn toml_version_value(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with('{') {
        // `{ version = "1", features = [...] }` or `{ path = "..." }`
        for part in raw.trim_matches(|c| c == '{' || c == '}').split(',') {
            if let Some((k, v)) = part.split_once('=') {
                if k.trim() == "version" {
                    return unquote(v);
                }
            }
        }
        if raw.contains("path") {
            return "path".to_string();
        }
        if raw.contains("git") {
            return "git".to_string();
        }
        return "*".to_string();
    }
    unquote(raw)
}

pub fn parse_cargo(content: &str) -> ManifestInfo {
    let mut m = empty_manifest("Cargo.toml");
    for (section, key, value) in toml_pairs(content) {
        match section.as_str() {
            "package" if key == "name" => m.name = Some(unquote(&value)),
            "package" if key == "version" => m.version = Some(unquote(&value)),
            "dependencies" | "workspace.dependencies" => {
                m.dependencies.insert(key, toml_version_value(&value));
            }
            "dev-dependencies" | "build-dependencies" => {
                m.dev_dependencies.insert(key, toml_version_value(&value));
            }
            _ => {}
        }
    }
    m
}

pub fn parse_pyproject(content: &str) -> ManifestInfo {
    let mut m = empty_manifest("pyproject.toml");
    let mut in_deps = false;
    let mut section = String::new();
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            section = t.trim_matches(|c| c == '[' || c == ']').to_string();
            in_deps = false;
            continue;
        }
        if in_deps {
            if t.starts_with(']') {
                in_deps = false;
                continue;
            }
            let dep = unquote(t.trim_end_matches(','));
            if !dep.is_empty() {
                let name: String = dep
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
                    .collect();
                let spec = dep[name.len()..].trim().to_string();
                m.dependencies.insert(name, if spec.is_empty() { "*".into() } else { spec });
            }
            continue;
        }
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match (section.as_str(), k) {
            ("project" | "tool.poetry", "name") => m.name = Some(unquote(v)),
            ("project" | "tool.poetry", "version") => m.version = Some(unquote(v)),
            ("project", "dependencies") if v.starts_with('[') => {
                if v.ends_with(']') {
                    for dep in v.trim_matches(|c| c == '[' || c == ']').split(',') {
                        let dep = unquote(dep);
                        if !dep.is_empty() {
                            m.dependencies.insert(dep, "*".into());
                        }
                    }
                } else {
                    in_deps = true;
                }
            }
            ("tool.poetry.dependencies", name) => {
                m.dependencies.insert(name.to_string(), toml_version_value(v));
            }
            ("project.scripts" | "tool.poetry.scripts", name) => {
                m.scripts.insert(name.to_string(), unquote(v));
            }
            _ => {}
        }
    }
    m
}

pub fn parse_go_mod(content: &str) -> ManifestInfo {
    let mut m = empty_manifest("go.mod");
    let mut in_require = false;
    for line in content.lines() {
        let t = line.trim();
        if let Some(module) = t.strip_prefix("module ") {
            m.name = Some(module.trim().to_string());
        } else if let Some(go) = t.strip_prefix("go ") {
            m.version = Some(go.trim().to_string());
        } else if t.starts_with("require (") {
            in_require = true;
        } else if in_require && t.starts_with(')') {
            in_require = false;
        } else if in_require || t.starts_with("require ") {
            let t = t.trim_start_matches("require ").trim();
            let mut parts = t.split_whitespace();
            if let (Some(name), Some(ver)) = (parts.next(), parts.next()) {
                if t.contains("// indirect") {
                    m.dev_dependencies.insert(name.to_string(), ver.to_string());
                } else {
                    m.dependencies.insert(name.to_string(), ver.to_string());
                }
            }
        }
    }
    m
}

// ------------------------------------------------------------------------ Package.swift

/// Drop `//` line comments and `/* */` block comments, keeping string literals intact.
fn strip_swift_comments(content: &str) -> String {
    let b: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let (mut i, mut in_str) = (0, false);
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// The text between the bracket at byte `open` and its matching closer (string-aware).
fn balanced_parens(s: &str, open: usize) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in s[open..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&s[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn swift_string_arg(body: &str, label: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(r#"\b{}\s*:\s*"([^"]*)""#, regex::escape(label))).ok()?;
    re.captures(body).map(|c| c[1].to_string())
}

/// The `label: [ ... ]` array body inside `body`, when present.
fn swift_array_arg<'a>(body: &'a str, label: &str) -> Option<&'a str> {
    let re = regex::Regex::new(&format!(r"\b{}\s*:\s*\[", regex::escape(label))).ok()?;
    let m = re.find(body)?;
    balanced_parens(body, m.end() - 1)
}

/// Dependency names in a target's `dependencies:` array: bare strings, `.target(name:)`,
/// `.byName(name:)` and `.product(name:, package:)` (rendered `package/name`).
fn swift_target_deps(arr: &str) -> Vec<String> {
    let product =
        regex::Regex::new(r#"\.product\s*\(\s*name\s*:\s*"([^"]+)"\s*,\s*package\s*:\s*"([^"]+)"[^)]*\)"#).unwrap();
    let named = regex::Regex::new(r#"\.(?:target|byName)\s*\(\s*name\s*:\s*"([^"]+)"[^)]*\)"#).unwrap();
    let quoted = regex::Regex::new(r#""([^"]+)""#).unwrap();
    let mut out: Vec<String> = Vec::new();
    for c in product.captures_iter(arr) {
        out.push(format!("{}/{}", &c[2], &c[1]));
    }
    for c in named.captures_iter(arr) {
        out.push(c[1].to_string());
    }
    let rest = product.replace_all(arr, "");
    let rest = named.replace_all(&rest, "");
    for c in quoted.captures_iter(&rest) {
        out.push(c[1].to_string());
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|d| seen.insert(d.clone()));
    out
}

/// Parse a SwiftPM `Package.swift` textually: package name, tools version, package
/// dependencies, products and targets (with their default source paths).
pub fn parse_package_swift(content: &str) -> ManifestInfo {
    let mut m = empty_manifest("Package.swift");
    m.version = content
        .lines()
        .next()
        .and_then(|l| l.split_once("swift-tools-version:"))
        .map(|(_, v)| v.trim().trim_end_matches(';').to_string())
        .filter(|v| !v.is_empty());
    let src = strip_swift_comments(content);
    let package_body = src
        .find("Package(")
        .and_then(|i| balanced_parens(&src, i + "Package".len()))
        .unwrap_or(&src);
    m.name = swift_string_arg(package_body, "name");

    // `.package(url: "https://github.com/apple/swift-argument-parser", from: "1.3.0")`
    let pkg = regex::Regex::new(r"\.package\s*\(").unwrap();
    let range = regex::Regex::new(r#""([^"]+)"\s*\.\.[.<]\s*"([^"]+)""#).unwrap();
    for mt in pkg.find_iter(&src) {
        let Some(body) = balanced_parens(&src, mt.end() - 1) else { continue };
        let Some(location) = swift_string_arg(body, "url").or_else(|| swift_string_arg(body, "path")) else {
            continue;
        };
        let name = swift_string_arg(body, "name").unwrap_or_else(|| {
            let trimmed = location.trim_end_matches('/').trim_end_matches(".git");
            trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
        });
        let version = ["from", "exact", "branch", "revision"]
            .iter()
            .find_map(|k| swift_string_arg(body, k).map(|v| if *k == "from" { format!(">={}", v) } else { v }))
            .or_else(|| range.captures(body).map(|c| format!("{}..<{}", &c[1], &c[2])))
            .unwrap_or_else(|| if body.contains("path:") { "path".into() } else { "*".into() });
        m.dependencies.insert(name, version);
    }

    // Products and targets: walk each `.kind(` call, skipping calls nested in an earlier one
    // (e.g. `.target(name:)` inside a target's dependency list).
    let call = regex::Regex::new(
        r"\.(library|executable|plugin|target|executableTarget|testTarget|macro|systemLibrary|binaryTarget)\s*\(",
    )
    .unwrap();
    let quoted = regex::Regex::new(r#""([^"]+)""#).unwrap();
    let mut consumed_until = 0;
    for c in call.captures_iter(&src) {
        let whole = c.get(0).unwrap();
        if whole.start() < consumed_until {
            continue;
        }
        let Some(body) = balanced_parens(&src, whole.end() - 1) else { continue };
        let Some(name) = swift_string_arg(body, "name") else { continue };
        consumed_until = whole.end() + body.len();
        let kind = &c[1];
        let is_product =
            matches!(kind, "library" | "executable") || (kind == "plugin" && !body.contains("capability:"));
        if is_product {
            let targets = swift_array_arg(body, "targets")
                .map(|a| quoted.captures_iter(a).map(|c| c[1].to_string()).collect())
                .unwrap_or_default();
            m.products.push(ManifestProduct { name, kind: kind.to_string(), targets });
        } else {
            let default_dir = match kind {
                "testTarget" => "Tests",
                "plugin" => "Plugins",
                _ => "Sources",
            };
            let path = swift_string_arg(body, "path").unwrap_or_else(|| format!("{}/{}", default_dir, name));
            let dependencies = swift_array_arg(body, "dependencies").map(swift_target_deps).unwrap_or_default();
            m.targets.push(ManifestTarget { name, kind: kind.to_string(), path, dependencies });
        }
    }
    m
}

/// Whether a Swift source declares a program entry (`@main` attribute on a type).
fn swift_has_main_attribute(content: &str) -> bool {
    content.lines().any(|l| {
        l.trim_start()
            .strip_prefix("@main")
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    })
}

fn is_swift_main_file(root: &Path, rel: &str) -> bool {
    fs::read_to_string(root.join(rel)).map(|c| swift_has_main_attribute(&c)).unwrap_or(false)
}

/// SwiftPM entry points: `main.swift` or the `@main` file of each executable target, each
/// test target directory, and `Package.swift` itself.
fn swiftpm_entry_points(root: &Path, files: &[&str]) -> Vec<EntryPoint> {
    let Ok(content) = fs::read_to_string(root.join("Package.swift")) else {
        return Vec::new();
    };
    let manifest = parse_package_swift(&content);
    let mut out = Vec::new();
    for t in &manifest.targets {
        let prefix = format!("{}/", t.path.trim_end_matches('/'));
        let swift_files = || files.iter().filter(|f| f.starts_with(&prefix) && f.ends_with(".swift"));
        match t.kind.as_str() {
            "executableTarget" => {
                let main = swift_files()
                    .find(|f| f.rsplit('/').next() == Some("main.swift"))
                    .or_else(|| swift_files().find(|f| is_swift_main_file(root, f)));
                if let Some(f) = main {
                    out.push(EntryPoint { path: f.to_string(), kind: "main".into() });
                }
            }
            "testTarget" => out.push(EntryPoint { path: t.path.clone(), kind: "test".into() }),
            _ => {}
        }
    }
    out.push(EntryPoint { path: "Package.swift".into(), kind: "config".into() });
    out
}

/// `@main` files of a Swift project without `Package.swift` (Xcode app targets), scanning a
/// bounded number of non-test `.swift` files.
fn swift_main_attribute_files(root: &Path, files: &[&str]) -> Vec<String> {
    files
        .iter()
        .filter(|f| f.ends_with(".swift") && !f.split('/').any(|seg| seg.ends_with("Tests")))
        .take(2000)
        .filter(|f| is_swift_main_file(root, f))
        .take(10)
        .map(|f| f.to_string())
        .collect()
}

// ------------------------------------------------------------------------- entry points

const MAIN_PATTERNS: &[&str] = &[
    "src/index.{ts,js,tsx,jsx}",
    "src/main.{ts,js,tsx,jsx,rs}",
    "src/lib.rs",
    "src/app.{ts,js,tsx,jsx}",
    "src/bin/*.rs",
    "app.{py,rb}",
    "main.{go,py,rs}",
    "index.{ts,js}",
    "server.{ts,js,py}",
    "src/cli.{ts,js}",
    "cmd/*/main.go",
    "manage.py",
    "__main__.py",
    "src/*/__main__.py",
];

const TEST_PATTERNS: &[&str] = &[
    "src/**/*.test.{ts,js,tsx,jsx}",
    "src/**/*.spec.{ts,js,tsx,jsx}",
    "tests/**/*.{ts,js,py,rs}",
    "test/**/*.{ts,js,py}",
    "**/*_test.go",
];

const CONFIG_PATTERNS: &[&str] = &[
    "tsconfig.json",
    "vite.config.{ts,js}",
    "next.config.{ts,js,mjs}",
    "webpack.config.{ts,js}",
    "jest.config.{ts,js}",
    "vitest.config.{ts,js}",
    ".eslintrc.{js,json,yml}",
    "eslint.config.{js,mjs}",
    "Cargo.toml",
    "rust-toolchain.toml",
    "clippy.toml",
    "rustfmt.toml",
    "pyproject.toml",
    "setup.cfg",
    "Dockerfile",
    "docker-compose.{yml,yaml}",
];

/// Expand `{a,b}` alternations (one level, possibly several groups).
pub fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(close_rel) = pattern[open..].find('}') else {
        return vec![pattern.to_string()];
    };
    let close = open + close_rel;
    let (head, alts, tail) = (&pattern[..open], &pattern[open + 1..close], &pattern[close + 1..]);
    alts.split(',')
        .flat_map(|alt| expand_braces(&format!("{}{}{}", head, alt, tail)))
        .collect()
}

pub fn scan_entry_points(root: &Path) -> Vec<EntryPoint> {
    let entries = walk_index(root, 6, IGNORE_DIRS);
    let files: Vec<&str> = {
        let mut f: Vec<&str> = entries.iter().filter(|e| !e.is_dir).map(|e| e.rel.as_str()).collect();
        f.sort();
        f
    };
    let mut out: Vec<EntryPoint> = Vec::new();
    let mut add = |patterns: &[&str], kind: &str, limit: Option<usize>| {
        let mut count = 0;
        for pattern in patterns {
            for p in expand_braces(pattern) {
                let Some(re) = glob_regex(&p) else { continue };
                for f in files.iter().filter(|f| re.is_match(f)) {
                    if out.iter().any(|e| e.path == *f) {
                        continue;
                    }
                    out.push(EntryPoint { path: f.to_string(), kind: kind.to_string() });
                    count += 1;
                    if limit.is_some_and(|l| count >= l) {
                        return;
                    }
                }
            }
        }
    };
    add(MAIN_PATTERNS, "main", None);
    add(TEST_PATTERNS, "test", Some(10));
    add(CONFIG_PATTERNS, "config", None);
    let mut swift = swiftpm_entry_points(root, &files);
    if swift.is_empty() && files.iter().any(|f| f.ends_with(".swift")) {
        swift = swift_main_attribute_files(root, &files)
            .into_iter()
            .map(|path| EntryPoint { path, kind: "main".into() })
            .collect();
    }
    for e in swift {
        if !out.iter().any(|o| o.path == e.path) {
            out.push(e);
        }
    }
    out
}

// -------------------------------------------------------------------------- folder tree

fn categorize(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| lower.starts_with(p));
    if starts(&["route", "page", "api", "endpoint"]) {
        "routes"
    } else if starts(&["model", "entities", "schema", "type"]) {
        "models"
    } else if starts(&["service", "provider", "handler", "controller", "action"]) {
        "services"
    } else if starts(&["test", "__tests__", "spec", "__spec__"]) {
        "tests"
    } else if starts(&["config", "setting"]) {
        "config"
    } else if starts(&["util", "helper", "lib", "shared", "common"]) {
        "utils"
    } else if starts(&["view", "component", "template", "layout", "ui"]) {
        "views"
    } else {
        "other"
    }
}

fn count_files(dir: &Path, depth: usize) -> usize {
    if depth > 3 {
        return 0;
    }
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || IGNORE_DIRS.contains(&name.as_str()) {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_file() => n += 1,
            Ok(t) if t.is_dir() => n += count_files(&e.path(), depth + 1),
            _ => {}
        }
    }
    n
}

pub fn scan_folder_tree(root: &Path) -> Vec<FolderCategory> {
    let Ok(rd) = fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<FolderCategory> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if (name.starts_with('.') && name != ".github") || IGNORE_DIRS.contains(&name.as_str()) {
                return None;
            }
            Some(FolderCategory {
                file_count: count_files(&e.path(), 0),
                category: categorize(&name).to_string(),
                path: name.clone(),
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| b.file_count.cmp(&a.file_count).then(a.name.cmp(&b.name)));
    out
}

// ------------------------------------------------------------------------------ tooling

fn any_exists(root: &Path, files: &[&str]) -> bool {
    files.iter().any(|f| root.join(f).exists())
}

fn file_contains(root: &Path, file: &str, needle: &str) -> bool {
    fs::read_to_string(root.join(file)).map(|c| c.contains(needle)).unwrap_or(false)
}

pub fn scan_tooling(root: &Path) -> ToolingInfo {
    let has_cargo = root.join("Cargo.toml").exists();
    let has_swiftpm = root.join("Package.swift").exists();
    let has_xcode = !has_swiftpm && xcode_project_name(root).is_some();
    let test_runner = if any_exists(root, &["vitest.config.ts", "vitest.config.js"]) {
        Some("vitest")
    } else if any_exists(root, &["jest.config.ts", "jest.config.js", "jest.config.json"]) {
        Some("jest")
    } else if any_exists(root, &["pytest.ini"]) || file_contains(root, "pyproject.toml", "pytest") {
        Some("pytest")
    } else if any_exists(root, &[".mocharc.yml", ".mocharc.json"]) {
        Some("mocha")
    } else if has_cargo {
        Some("cargo test")
    } else if root.join("go.mod").exists() {
        Some("go test")
    } else if root.join("pyproject.toml").exists() {
        Some("pytest")
    } else if has_swiftpm {
        Some("swift test")
    } else if has_xcode {
        Some("xcodebuild test")
    } else {
        None
    };
    let build_tool = if any_exists(root, &["tsup.config.ts", "tsup.config.js"]) {
        Some("tsup")
    } else if any_exists(root, &["vite.config.ts", "vite.config.js"]) {
        Some("vite")
    } else if any_exists(root, &["next.config.ts", "next.config.js", "next.config.mjs"]) {
        Some("next")
    } else if any_exists(root, &["webpack.config.ts", "webpack.config.js"]) {
        Some("webpack")
    } else if any_exists(root, &["rollup.config.ts", "rollup.config.js"]) {
        Some("rollup")
    } else if any_exists(root, &["esbuild.config.ts"]) {
        Some("esbuild")
    } else if has_cargo {
        Some("cargo")
    } else if has_swiftpm {
        Some("swift build")
    } else if has_xcode {
        Some("xcodebuild")
    } else if any_exists(root, &["Makefile"]) {
        Some("make")
    } else {
        None
    };
    let linter = if any_exists(
        root,
        &["eslint.config.js", "eslint.config.mjs", ".eslintrc.js", ".eslintrc.json", ".eslintrc.yml"],
    ) {
        Some("eslint")
    } else if any_exists(root, &["ruff.toml", ".ruff.toml"]) || file_contains(root, "pyproject.toml", "[tool.ruff") {
        Some("ruff")
    } else if any_exists(root, &[".pylintrc", "pylintrc"]) {
        Some("pylint")
    } else if any_exists(root, &[".flake8"]) {
        Some("flake8")
    } else if any_exists(root, &[".golangci.yml"]) {
        Some("golangci-lint")
    } else if any_exists(root, &[".swiftlint.yml", ".swiftlint.yaml"]) {
        Some("swiftlint")
    } else if has_cargo {
        Some("clippy")
    } else {
        None
    };
    let formatter = if any_exists(root, &[".prettierrc", ".prettierrc.json", ".prettierrc.js", "prettier.config.js"]) {
        Some("prettier")
    } else if any_exists(root, &["biome.json"]) {
        Some("biome")
    } else if any_exists(root, &[".swift-format"]) {
        Some("swift-format")
    } else if any_exists(root, &[".swiftformat"]) {
        Some("swiftformat")
    } else if any_exists(root, &["rustfmt.toml", ".rustfmt.toml"]) || has_cargo {
        Some("rustfmt")
    } else if file_contains(root, "pyproject.toml", "[tool.black") {
        Some("black")
    } else if any_exists(root, &[".editorconfig"]) {
        Some("editorconfig")
    } else {
        None
    };
    let package_manager = if any_exists(root, &["bun.lockb", "bun.lock"]) {
        Some("bun")
    } else if any_exists(root, &["pnpm-lock.yaml"]) {
        Some("pnpm")
    } else if any_exists(root, &["yarn.lock"]) {
        Some("yarn")
    } else if any_exists(root, &["package-lock.json", "package.json"]) {
        Some("npm")
    } else if has_cargo {
        Some("cargo")
    } else if any_exists(root, &["poetry.lock"]) {
        Some("poetry")
    } else if any_exists(root, &["uv.lock"]) {
        Some("uv")
    } else if root.join("go.mod").exists() {
        Some("go")
    } else if any_exists(root, &["requirements.txt", "pyproject.toml"]) {
        Some("pip")
    } else if has_swiftpm {
        Some("swiftpm")
    } else {
        None
    };
    ToolingInfo {
        test_runner: test_runner.map(str::to_string),
        build_tool: build_tool.map(str::to_string),
        linter: linter.map(str::to_string),
        formatter: formatter.map(str::to_string),
        package_manager: package_manager.map(str::to_string),
    }
}

// ------------------------------------------------------------------------------- readme

pub const README_MAX_CHARS: usize = 3000;

pub fn scan_readme(root: &Path) -> Option<String> {
    for name in ["README.md", "readme.md", "Readme.md", "README"] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        let content = fs::read_to_string(path).ok()?;
        if content.chars().count() > README_MAX_CHARS {
            let truncated: String = content.chars().take(README_MAX_CHARS).collect();
            return Some(format!("{}\n... (truncated)", truncated));
        }
        return Some(content);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braces_expand() {
        assert_eq!(expand_braces("a.{ts,js}"), vec!["a.ts", "a.js"]);
        assert_eq!(expand_braces("{x,y}/m.{a,b}").len(), 4);
        assert_eq!(expand_braces("plain"), vec!["plain"]);
    }

    #[test]
    fn cargo_and_go_parse() {
        let c = parse_cargo("[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1.0\", features = [\"derive\"] }\nregex = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n");
        assert_eq!(c.name.as_deref(), Some("demo"));
        assert_eq!(c.dependencies.get("serde").map(String::as_str), Some("1.0"));
        assert_eq!(c.dev_dependencies.get("tempfile").map(String::as_str), Some("3"));
        let g = parse_go_mod("module example.com/svc\n\ngo 1.22\n\nrequire (\n\tgithub.com/a/b v1.2.3\n\tgithub.com/c/d v0.1.0 // indirect\n)\n");
        assert_eq!(g.name.as_deref(), Some("example.com/svc"));
        assert_eq!(g.dependencies.len(), 1);
        let p = parse_pyproject("[project]\nname = \"pkg\"\nversion = \"2.0\"\ndependencies = [\n  \"requests>=2\",\n  \"click\",\n]\n");
        assert_eq!(p.dependencies.get("requests").map(String::as_str), Some(">=2"));
        assert!(p.dependencies.contains_key("click"));
    }
}
