//! TypeScript / JavaScript module resolution from `tsconfig.json` / `jsconfig.json`:
//! `compilerOptions.baseUrl` and `compilerOptions.paths` (with `extends` chains of relative
//! config files), applied to every source file under the config's directory (the nearest
//! enclosing config wins, which approximates the compiler's `include` sets).
//!
//! This is a syntactic approximation of the TypeScript compiler's module resolution: package
//! `exports` maps, `node_modules` lookups, `rootDirs`, project references and `include`/`exclude`
//! globs are not evaluated. Non-relative specifiers that no `paths`/`baseUrl` rule maps to a file
//! of the corpus stay external modules.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default)]
struct TsConfig {
    /// Directory of the config (corpus-relative, `""` for the root).
    dir: String,
    /// Corpus-relative base URL directory, if any.
    base_url: Option<String>,
    /// `paths` patterns with their substitutions, already relative to the corpus root.
    paths: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TsConfigs {
    configs: Vec<TsConfig>,
}

/// Remove `//` and `/* */` comments (outside strings) and trailing commas.
pub(crate) fn strip_jsonc(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
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
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&'/') {
            while i < bytes.len() && bytes[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == '*' && bytes[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Trailing commas: `,` followed only by whitespace before `}` or `]`.
    let chars: Vec<char> = out.chars().collect();
    let mut cleaned = String::with_capacity(out.len());
    let mut in_str = false;
    for (k, &c) in chars.iter().enumerate() {
        if in_str {
            cleaned.push(c);
            if c == '"' && (k == 0 || chars[k - 1] != '\\') {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        if c == ',' {
            let next = chars[k + 1..].iter().find(|ch| !ch.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        cleaned.push(c);
    }
    cleaned
}

fn parent_dir(file: &str) -> &str {
    file.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Join and normalise `.`/`..` components of a `/`-separated relative path.
pub(crate) fn normalize_join(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if rel.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|s| !s.is_empty()).collect()
    };
    for comp in rel.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// `paths` rules: (pattern, substitutions).
type PathRules = Vec<(String, Vec<String>)>;
/// `paths` with the directory of the config that declared them.
type DeclaredPaths = Option<(String, PathRules)>;

/// Effective (`baseUrl`, `paths`) of a config file, following relative `extends`.
fn load_config(
    root: &Path,
    rel: &str,
    depth: usize,
) -> Option<(Option<String>, DeclaredPaths)> {
    if depth > 5 {
        return None;
    }
    let text = std::fs::read_to_string(root.join(rel)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&strip_jsonc(&text)).ok()?;
    let dir = parent_dir(rel).to_string();
    let mut base_url: Option<String> = None;
    // `paths` are relative to `baseUrl`, else to the config declaring them.
    let mut paths: DeclaredPaths = None;
    if let Some(ext) = value.get("extends").and_then(|e| e.as_str()) {
        if ext.starts_with('.') {
            let mut target = normalize_join(&dir, ext);
            if !target.ends_with(".json") {
                target.push_str(".json");
            }
            if let Some((b, p)) = load_config(root, &target, depth + 1) {
                base_url = b;
                paths = p;
            }
        }
    }
    let opts = value.get("compilerOptions");
    if let Some(b) = opts.and_then(|o| o.get("baseUrl")).and_then(|b| b.as_str()) {
        base_url = Some(normalize_join(&dir, b));
    }
    if let Some(p) = opts.and_then(|o| o.get("paths")).and_then(|p| p.as_object()) {
        let mut list = Vec::new();
        for (pattern, subs) in p {
            let subs: Vec<String> = subs
                .as_array()
                .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                .unwrap_or_default();
            list.push((pattern.clone(), subs));
        }
        paths = Some((dir.clone(), list));
    }
    Some((base_url, paths))
}

impl TsConfigs {
    /// Configs found among `files` (corpus-relative paths) plus root-level ones on disk.
    pub(crate) fn load<'a>(root: &Path, files: impl Iterator<Item = &'a String>) -> Self {
        let mut rels: Vec<String> = files
            .filter(|f| {
                let name = f.rsplit('/').next().unwrap_or(f);
                (name.starts_with("tsconfig") && name.ends_with(".json")) || name == "jsconfig.json"
            })
            .filter(|f| !f.contains("node_modules/"))
            .cloned()
            .collect();
        for name in ["tsconfig.json", "jsconfig.json"] {
            if root.join(name).is_file() && !rels.iter().any(|r| r == name) {
                rels.push(name.to_string());
            }
        }
        // Prefer the canonical `tsconfig.json` of a directory over variants (`tsconfig.build.json`).
        rels.sort_by_key(|r| {
            let name = r.rsplit('/').next().unwrap_or(r);
            (parent_dir(r).to_string(), name != "tsconfig.json", name.to_string())
        });
        let mut configs: Vec<TsConfig> = Vec::new();
        for rel in rels {
            let dir = parent_dir(&rel).to_string();
            if configs.iter().any(|c| c.dir == dir) {
                continue;
            }
            let Some((base_url, paths)) = load_config(root, &rel, 0) else { continue };
            let paths = paths
                .map(|(decl_dir, list)| {
                    let base = base_url.clone().unwrap_or(decl_dir);
                    list.into_iter()
                        .map(|(pat, subs)| {
                            (pat, subs.iter().map(|s| normalize_join(&base, s)).collect())
                        })
                        .collect()
                })
                .unwrap_or_default();
            if base_url.is_none() && Vec::<(String, Vec<String>)>::is_empty(&paths) {
                continue;
            }
            configs.push(TsConfig { dir, base_url, paths });
        }
        TsConfigs { configs }
    }

    fn config_for(&self, file: &str) -> Option<&TsConfig> {
        self.configs
            .iter()
            .filter(|c| c.dir.is_empty() || file.starts_with(&format!("{}/", c.dir)))
            .max_by_key(|c| c.dir.len())
    }

    /// Corpus-relative candidate module paths (without extension handling) for a non-relative
    /// specifier imported from `file`, in priority order.
    pub(crate) fn candidates(&self, file: &str, spec: &str) -> Vec<String> {
        let Some(cfg) = self.config_for(file) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // The longest matching prefix pattern wins, as in the compiler.
        let mut best: Option<(usize, &Vec<String>, String)> = None;
        for (pat, subs) in &cfg.paths {
            if let Some((pre, suf)) = pat.split_once('*') {
                if spec.len() >= pre.len() + suf.len() && spec.starts_with(pre) && spec.ends_with(suf) {
                    let star = spec[pre.len()..spec.len() - suf.len()].to_string();
                    if best.as_ref().is_none_or(|(len, _, _)| pre.len() > *len) {
                        best = Some((pre.len(), subs, star));
                    }
                }
            } else if pat == spec {
                best = Some((usize::MAX, subs, String::new()));
                break;
            }
        }
        if let Some((_, subs, star)) = best {
            for s in subs {
                out.push(s.replace('*', &star));
            }
        }
        if let Some(b) = &cfg.base_url {
            out.push(normalize_join(b, spec));
        }
        let mut dedup: HashMap<String, ()> = HashMap::new();
        out.retain(|c| dedup.insert(c.clone(), ()).is_none());
        out
    }
}
