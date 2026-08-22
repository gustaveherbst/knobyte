//! DEPENDENCY_MISSING / VERSION_MISMATCH: dependencies the scaffold names must be declared in
//! a manifest (package.json, nested */package.json, pyproject.toml, Cargo.toml, Package.swift),
//! and claimed
//! versions must agree with the declared ones.

use std::fs;
use std::sync::OnceLock;

use regex::Regex;

use super::cross_file::version_claim_re;
use super::{read_json, CheckContext};
use crate::drift::types::{codes, Claim, ClaimKind, DriftIssue, SEVERITY_WARNING};

/// Runtimes, platforms, databases, protocols and tools that stack docs name but that are not
/// installable packages.
const KNOWN_RUNTIMES: &[&str] = &[
    "node.js",
    "node",
    "nodejs",
    "python",
    "cpython",
    "go",
    "golang",
    "rust",
    "ruby",
    "java",
    "jdk",
    "jre",
    "deno",
    "bun",
    "swift",
    "kotlin",
    "elixir",
    "erlang",
    "php",
    ".net",
    "dotnet",
    "c#",
    "csharp",
    "sqlite",
    "sqlite3",
    "postgresql",
    "postgres",
    "mysql",
    "mariadb",
    "mongodb",
    "mongo",
    "redis",
    "elasticsearch",
    "dynamodb",
    "cassandra",
    "neo4j",
    "supabase",
    "neon",
    "docker",
    "kubernetes",
    "k8s",
    "vercel",
    "netlify",
    "railway",
    "fly.io",
    "render",
    "aws",
    "gcp",
    "azure",
    "cloudflare",
    "s3",
    "ec2",
    "lambda",
    "ecs",
    "fargate",
    "rest",
    "rest api",
    "graphql",
    "grpc",
    "websocket",
    "websockets",
    "oauth",
    "oauth2",
    "jwt",
    "saml",
    "oidc",
    "http",
    "https",
    "tcp",
    "udp",
    "tailwind",
    "tailwind css",
    "tailwindcss",
    "bootstrap",
    "sass",
    "less",
    "postcss",
    "webpack",
    "vite",
    "esbuild",
    "turbopack",
    "rollup",
    "parcel",
    "git",
    "github",
    "gitlab",
    "ci/cd",
    "nginx",
    "apache",
    "caddy",
    "npm",
    "pnpm",
    "yarn",
    "npx",
    "corepack",
    "linux",
    "macos",
    "windows",
    "wasm",
    "webassembly",
];

/// Architectural labels that name a part of a system rather than something installable.
const NON_PACKAGE_LABELS: &[&str] = &[
    "frontend",
    "backend",
    "fullstack",
    "full-stack",
    "database",
    "storage",
    "persistence",
    "middleware",
    "infrastructure",
    "infra",
    "platform",
    "authentication",
    "authorization",
    "caching",
    "queue",
    "queues",
    "scheduler",
    "workers",
    "server",
    "client",
    "monorepo",
    "tooling",
    "observability",
    "testing",
    "deployment",
    "orchestration",
    "gateway",
    "firewall",
];

fn concept_acronym() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Z][A-Z0-9]*$").unwrap())
}

/// One declared dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEntry {
    pub name: String,
    pub version: String,
    /// Names compare after normalisation (`-`, `_`, `.` equivalent): PyPI and crates.io.
    pub normalizes: bool,
}

fn normalize_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_sep = false;
    for c in lower.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !last_sep {
                out.push('-');
            }
            last_sep = true;
        } else {
            out.push(c);
            last_sep = false;
        }
    }
    out
}

fn find_dependency<'a>(deps: &'a [DepEntry], claimed: &str) -> Option<&'a DepEntry> {
    if let Some(exact) = deps.iter().find(|d| d.name.to_lowercase() == claimed) {
        return Some(exact);
    }
    let normalized = normalize_name(claimed);
    deps.iter()
        .find(|d| d.normalizes && normalize_name(&d.name) == normalized)
}

pub fn check_dependencies(claims: &[Claim], ctx: &CheckContext) -> Vec<DriftIssue> {
    let mut issues = Vec::new();
    let Some(deps) = load_all_dependencies(ctx) else {
        return issues;
    };

    for claim in claims
        .iter()
        .filter(|c| c.kind == ClaimKind::Dependency && !c.negated)
    {
        let name = claim.value.to_lowercase();
        if KNOWN_RUNTIMES.contains(&name.as_str())
            || NON_PACKAGE_LABELS.contains(&name.as_str())
            || concept_acronym().is_match(&claim.value)
        {
            continue;
        }
        if find_dependency(&deps, &name).is_none() {
            issues.push(DriftIssue::from_claim(
                codes::DEPENDENCY_MISSING,
                SEVERITY_WARNING,
                claim,
                format!(
                    "Claimed dependency \"{}\" not found in any manifest",
                    claim.value
                ),
            ));
        }
    }

    for claim in claims
        .iter()
        .filter(|c| c.kind == ClaimKind::Version && !c.negated)
    {
        let Some(caps) = version_claim_re().captures(&claim.value) else {
            continue;
        };
        let name = caps[1].trim().to_lowercase();
        let claimed_version = &caps[2];
        if let Some(found) = find_dependency(&deps, &name) {
            if !found.version.contains(claimed_version) {
                issues.push(DriftIssue::from_claim(
                    codes::VERSION_MISMATCH,
                    SEVERITY_WARNING,
                    claim,
                    format!(
                        "Claimed \"{}\" but manifest has version \"{}\"",
                        claim.value, found.version
                    ),
                ));
            }
        }
    }
    issues
}

fn package_json_deps(value: &serde_json::Value, out: &mut Vec<DepEntry>) {
    for key in ["dependencies", "devDependencies"] {
        if let Some(obj) = value.get(key).and_then(|v| v.as_object()) {
            for (name, version) in obj {
                let version = match version {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.push(DepEntry {
                    name: name.clone(),
                    version,
                    normalizes: false,
                });
            }
        }
    }
}

/// Every dependency the project's manifests declare; `None` when none could be read (which
/// switches the checker off rather than measuring claims against an empty list).
pub fn load_all_dependencies(ctx: &CheckContext) -> Option<Vec<DepEntry>> {
    let root = &ctx.project_root;
    let mut entries = Vec::new();

    if let Some(pkg) = read_json(&root.join("package.json")) {
        package_json_deps(&pkg, &mut entries);
    }
    if let Ok(content) = fs::read_to_string(root.join("pyproject.toml")) {
        entries.extend(parse_pyproject_dependencies(&content));
    }
    if let Ok(content) = fs::read_to_string(root.join("Cargo.toml")) {
        entries.extend(parse_cargo_dependencies(&content));
    }
    if let Ok(content) = fs::read_to_string(root.join("Package.swift")) {
        entries.extend(parse_swift_package_dependencies(&content));
    }
    // Nested applications (`*/package.json`, `*/Cargo.toml`, `*/Package.swift`) one level down.
    if let Ok(rd) = fs::read_dir(root) {
        let mut dirs: Vec<_> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter(|e| e.file_name() != "node_modules")
            .map(|e| e.path())
            .collect();
        dirs.sort();
        for dir in dirs {
            if let Some(pkg) = read_json(&dir.join("package.json")) {
                package_json_deps(&pkg, &mut entries);
            }
            if let Ok(content) = fs::read_to_string(dir.join("Cargo.toml")) {
                entries.extend(parse_cargo_dependencies(&content));
            }
            if let Ok(content) = fs::read_to_string(dir.join("Package.swift")) {
                entries.extend(parse_swift_package_dependencies(&content));
            }
        }
    }

    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

/// Blank every quoted run so brackets, commas and `#` inside strings are not structure.
fn mask_strings(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            if let Some(rel) = chars[i + 1..].iter().position(|x| *x == c) {
                out.push(c);
                for _ in 0..rel {
                    out.push(' ');
                }
                out.push(c);
                i += rel + 2;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn strip_comment(line: &str) -> String {
    let mask = mask_strings(line);
    match mask.chars().position(|c| c == '#') {
        Some(idx) => line
            .chars()
            .take(idx)
            .collect::<String>()
            .trim()
            .to_string(),
        None => line.trim().to_string(),
    }
}

/// Split an array body on top-level commas (not those inside strings or nested brackets).
fn split_items(body: &str) -> Vec<String> {
    let mask: Vec<char> = mask_strings(body).chars().collect();
    let chars: Vec<char> = body.chars().collect();
    let mut items = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    for (i, ch) in mask.iter().enumerate() {
        match ch {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                items.push(chars[start..i].iter().collect());
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(chars[start..].iter().collect());
    items
}

fn quoted_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""([^"]*)"|'([^']*)'"#).unwrap())
}

fn spec_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([A-Za-z0-9][\w.-]*)").unwrap())
}

/// Package name and raw specifier from one PEP 508 array item.
fn spec_entry(item: &str) -> Option<DepEntry> {
    let text = item.trim();
    if text.is_empty() || text.starts_with('{') {
        return None;
    }
    let caps = quoted_re().captures(text)?;
    let spec = caps.get(1).or_else(|| caps.get(2))?.as_str();
    let name = spec_name_re().captures(spec)?.get(1)?.as_str().to_string();
    let rest = spec[name.len()..].trim();
    Some(DepEntry {
        version: if rest.is_empty() {
            "*".to_string()
        } else {
            rest.to_string()
        },
        name,
        normalizes: true,
    })
}

fn header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[{1,2}([^\]]+)\]{1,2}$").unwrap())
}

fn key_value_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^["']?([A-Za-z0-9][\w.-]*)["']?\s*=\s*(.*)$"#).unwrap())
}

fn inline_version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bversion\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap())
}

fn poetry_table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^tool\.poetry(?:\.group\.[\w.-]+)?\.dependencies$").unwrap())
}

fn trim_quotes(s: &str) -> String {
    s.trim()
        .trim_end_matches(',')
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string()
}

#[derive(PartialEq)]
enum Open {
    Collect,
    Skip,
}

/// Dependency names from a pyproject.toml: `[project] dependencies`,
/// `[project.optional-dependencies]`, `[dependency-groups]` and the poetry tables.
pub fn parse_pyproject_dependencies(content: &str) -> Vec<DepEntry> {
    let mut entries = Vec::new();
    let mut table = String::new();
    let mut project_name = String::new();
    let mut open: Option<Open> = None;

    for raw in content.lines() {
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if let Some(mode) = &open {
            let mask = mask_strings(&line);
            let closed = mask.contains(']') || mask.contains('}');
            if *mode == Open::Collect {
                let body = trim_closing(&line);
                for item in split_items(&body) {
                    if let Some(e) = spec_entry(&item) {
                        entries.push(e);
                    }
                }
            }
            if closed {
                open = None;
            }
            continue;
        }
        if let Some(caps) = header_re().captures(&line) {
            table = caps[1].trim().to_string();
            continue;
        }
        let Some(caps) = key_value_re().captures(&line) else {
            continue;
        };
        let key = caps[1].to_string();
        let value = caps[2].trim().to_string();

        if table == "project" && key == "name" {
            project_name = trim_quotes(&value);
            continue;
        }
        if poetry_table_re().is_match(&table) {
            if key.eq_ignore_ascii_case("python") {
                continue;
            }
            let version = inline_version_re()
                .captures(&value)
                .and_then(|c| {
                    c.get(1)
                        .or_else(|| c.get(2))
                        .map(|m| m.as_str().to_string())
                })
                .unwrap_or_else(|| {
                    let v = trim_quotes(&value);
                    if v.is_empty() {
                        "*".to_string()
                    } else {
                        v
                    }
                });
            entries.push(DepEntry {
                name: key,
                version,
                normalizes: true,
            });
            let mask = mask_strings(&value);
            if !(mask.contains(']') || mask.contains('}'))
                && (value.starts_with('[') || value.starts_with('{'))
            {
                open = Some(Open::Skip);
            }
            continue;
        }
        let is_dep_array = (table == "project" && key == "dependencies")
            || table == "project.optional-dependencies"
            || table == "dependency-groups";
        if !is_dep_array {
            continue;
        }
        let Some(bracket) = value.find('[') else {
            continue;
        };
        let body = &value[bracket + 1..];
        for item in split_items(&trim_closing(body)) {
            if let Some(e) = spec_entry(&item) {
                entries.push(e);
            }
        }
        if !mask_strings(body).contains(']') {
            open = Some(Open::Collect);
        }
    }

    if project_name.is_empty() {
        entries
    } else {
        let me = normalize_name(&project_name);
        entries
            .into_iter()
            .filter(|e| normalize_name(&e.name) != me)
            .collect()
    }
}

/// Remove a trailing `]`/`}` (and following commas/space) that closes an array.
fn trim_closing(s: &str) -> String {
    let t = s.trim_end();
    let t = t.trim_end_matches(|c: char| c == ',' || c.is_whitespace());
    let t = t
        .strip_suffix(']')
        .or_else(|| t.strip_suffix('}'))
        .unwrap_or(t);
    t.to_string()
}

fn cargo_dep_table(table: &str) -> bool {
    let t = table.trim();
    matches!(
        t,
        "dependencies" | "dev-dependencies" | "build-dependencies" | "workspace.dependencies"
    ) || (t.starts_with("target.")
        && (t.ends_with(".dependencies")
            || t.ends_with(".dev-dependencies")
            || t.ends_with(".build-dependencies")))
}

/// Dependency names from a Cargo.toml dependency table (`name = "1.0"` or
/// `name = { version = "1.0", ... }`; `[dependencies.name]` sub-tables too).
pub fn parse_cargo_dependencies(content: &str) -> Vec<DepEntry> {
    let mut entries = Vec::new();
    let mut table = String::new();
    // `[dependencies.foo]` style: name plus the version found inside.
    let mut subtable_dep: Option<usize> = None;

    for raw in content.lines() {
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if let Some(caps) = header_re().captures(&line) {
            table = caps[1].trim().to_string();
            subtable_dep = None;
            if let Some((parent, name)) = table.rsplit_once('.') {
                if cargo_dep_table(parent) {
                    entries.push(DepEntry {
                        name: trim_quotes(name),
                        version: "*".to_string(),
                        normalizes: true,
                    });
                    subtable_dep = Some(entries.len() - 1);
                }
            }
            continue;
        }
        let Some(caps) = key_value_re().captures(&line) else {
            continue;
        };
        let key = caps[1].to_string();
        let value = caps[2].trim().to_string();
        if let Some(idx) = subtable_dep {
            if key == "version" {
                entries[idx].version = trim_quotes(&value);
            }
            continue;
        }
        if !cargo_dep_table(&table) {
            continue;
        }
        let version = if value.starts_with('{') {
            inline_version_re()
                .captures(&value)
                .and_then(|c| {
                    c.get(1)
                        .or_else(|| c.get(2))
                        .map(|m| m.as_str().to_string())
                })
                .unwrap_or_else(|| "*".to_string())
        } else {
            trim_quotes(&value)
        };
        entries.push(DepEntry {
            name: key,
            version,
            normalizes: true,
        });
    }
    entries
}

fn swift_package_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\.package\s*\(").unwrap())
}

fn swift_product_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\.product\s*\(\s*name\s*:\s*"([^"]+)""#).unwrap())
}

fn swift_arg_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\b(name|url|path|id|from|exact|branch|revision)\s*:\s*"([^"]*)""#).unwrap()
    })
}

fn swift_range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""([^"]+)"\s*(\.\.[.<])\s*"([^"]+)""#).unwrap())
}

/// The text inside the balanced `( .. )` that opens at byte `open`.
fn balanced_args(content: &str, open: usize) -> &str {
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, ch) in content[open..].char_indices() {
        match ch {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return &content[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    &content[open + 1..]
}

/// Package name a SwiftPM dependency location implies: the last path segment of a URL or
/// path without `.git`, or the name part of an `owner.name` registry id.
fn swift_location_name(location: &str) -> String {
    let tail = location.trim_end_matches('/').trim_end_matches(".git");
    let last = tail.rsplit('/').next().unwrap_or(tail);
    if location.contains('/') {
        last.to_string()
    } else {
        last.rsplit('.').next().unwrap_or(last).to_string()
    }
}

/// SwiftPM `Package.swift` source with `//` line comments removed.
fn strip_swift_comments(content: &str) -> String {
    content
        .lines()
        .map(|l| match l.find("//") {
            // A `//` inside a string literal (`https://..`) is not a comment.
            Some(i) if l[..i].matches('"').count() % 2 == 0 => &l[..i],
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dependencies a SwiftPM `Package.swift` declares: each `.package(url:|path:|id: ..)` (named
/// by its explicit `name:` or after its repository) plus every `.product(name:)` a target
/// depends on, since docs usually name the product (`ArgumentParser`) rather than the package
/// (`swift-argument-parser`).
pub fn parse_swift_package_dependencies(content: &str) -> Vec<DepEntry> {
    let code = strip_swift_comments(content);
    let mut entries = Vec::new();
    for m in swift_package_re().find_iter(&code) {
        let args = balanced_args(&code, m.end() - 1);
        let mut name = None;
        let mut location = None;
        let mut version = None;
        for caps in swift_arg_re().captures_iter(args) {
            let value = caps[2].to_string();
            match &caps[1] {
                "name" => name = Some(value),
                "url" | "path" | "id" => location = location.or(Some(value)),
                "from" => version = version.or(Some(format!(">={}", value))),
                _ => version = version.or(Some(value)),
            }
        }
        if version.is_none() {
            version = swift_range_re()
                .captures(args)
                .map(|c| format!("{}{}{}", &c[1], &c[2], &c[3]));
        }
        let name = name.or_else(|| location.as_deref().map(swift_location_name));
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            entries.push(DepEntry {
                name,
                version: version.unwrap_or_else(|| "*".to_string()),
                normalizes: true,
            });
        }
    }
    for caps in swift_product_re().captures_iter(&code) {
        let name = caps[1].to_string();
        if !entries.iter().any(|e| e.name == name) {
            entries.push(DepEntry {
                name,
                version: "*".to_string(),
                normalizes: true,
            });
        }
    }
    entries
}

/// Executable products and executable targets a SwiftPM `Package.swift` declares — the names
/// `swift run <name>` accepts.
pub(crate) fn swift_executables(content: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"\.(?:executable|executableTarget)\s*\(\s*name\s*:\s*"([^"]+)""#).unwrap()
    });
    re.captures_iter(&strip_swift_comments(content))
        .map(|c| c[1].to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyproject_shapes() {
        let toml = r#"
[project]
name = "myapp"
dependencies = [
  "fastapi>=0.115",
  "celery[redis]>=5, <6",  # comment
]

[project.optional-dependencies]
all = ["myapp[extra]", "rich"]

[tool.poetry.dependencies]
python = "^3.12"
httpx = { version = "^0.27", optional = true }
"#;
        let names: Vec<String> = parse_pyproject_dependencies(toml)
            .into_iter()
            .map(|e| format!("{}={}", e.name, e.version))
            .collect();
        assert_eq!(
            names,
            vec![
                "fastapi=>=0.115",
                "celery=[redis]>=5, <6",
                "rich=*",
                "httpx=^0.27"
            ]
        );
    }

    #[test]
    fn cargo_shapes() {
        let toml = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1.0\", features = [\"derive\"] }\ntokio = \"1.43\"\n\n[dev-dependencies.tempfile]\nversion = \"3.17\"\n";
        let names: Vec<String> = parse_cargo_dependencies(toml)
            .into_iter()
            .map(|e| format!("{}={}", e.name, e.version))
            .collect();
        assert_eq!(names, vec!["serde=1.0", "tokio=1.43", "tempfile=3.17"]);
    }

    #[test]
    fn swift_package_shapes() {
        let manifest = r#"// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Tool",
    products: [.executable(name: "tool", targets: ["Tool"])],
    dependencies: [
        .package(url: "https://github.com/apple/swift-argument-parser.git", from: "1.3.0"),
        .package(url: "https://github.com/vapor/vapor", exact: "4.92.1"),
        .package(name: "Local", path: "../Local"),
        .package(id: "pointfree.swift-snapshot-testing", "1.10.0"..<"2.0.0"),
        // .package(url: "https://github.com/commented/out", from: "1.0.0"),
    ],
    targets: [
        .executableTarget(name: "Tool", dependencies: [
            .product(name: "ArgumentParser", package: "swift-argument-parser"),
        ]),
    ]
)
"#;
        let names: Vec<String> = parse_swift_package_dependencies(manifest)
            .into_iter()
            .map(|e| format!("{}={}", e.name, e.version))
            .collect();
        assert_eq!(
            names,
            vec![
                "swift-argument-parser=>=1.3.0",
                "vapor=4.92.1",
                "Local=*",
                "swift-snapshot-testing=1.10.0..<2.0.0",
                "ArgumentParser=*"
            ]
        );
        assert_eq!(swift_executables(manifest), vec!["tool", "Tool"]);
    }
}
