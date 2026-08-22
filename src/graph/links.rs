//! Cross-file structure facts linked at build time.
//!
//! Extraction is per file, so facts that only the whole corpus can settle are emitted as link
//! references (`link:*` kinds, see [`crate::graph::extractor::LINK_PREFIX`]) and resolved here:
//!
//! - Rust `mod name;` declarations: the namespace node is linked (`contains`, method
//!   `rust-mod-decl`) to the file holding the module (`name.rs` / `name/mod.rs`, or the
//!   `#[path]` file).
//! - Express router mounts: `app.use('/api', router)` with `router` declared in the same file,
//!   imported (`import r from './routes'`, `const r = require('./routes')`, through re-exports)
//!   or required inline prefixes every route of that router; so does a mount onto an imported
//!   router and a route declared on one. Mounts compose transitively across any number of files
//!   and levels (`/api` + `/v1` + `/users`), so route nodes carry their full path; cycles are cut
//!   (each router is visited once per chain, at most 16 levels). A router mounted at several
//!   prefixes yields one route node per mounted path.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::graph::extractor::{
    ExtractionResult, LINK_EXPRESS_EXPORT, LINK_EXPRESS_MOUNT, LINK_EXPRESS_ROUTE,
};
use crate::graph::models::ExtractedRef;

/// Longest mount chain followed (guards against cycles and pathological nesting).
const MAX_MOUNT_DEPTH: usize = 16;
/// Most mounted paths kept per router.
const MAX_PREFIXES: usize = 16;

const JS_EXTENSIONS: [&str; 8] = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Join and normalise `base` + `rel` (POSIX, project-relative). `None` when `..` escapes.
fn join_path(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// A relative JS/TS module specifier resolved against the indexed files.
pub(crate) fn resolve_relative_js(from_file: &str, spec: &str, files: &HashSet<String>) -> Option<String> {
    if !spec.starts_with('.') {
        return None;
    }
    let base = join_path(dirname(from_file), spec)?;
    let mut stems = vec![base.clone()];
    // ESM TypeScript writes `./users.js` for `users.ts`.
    for ext in JS_EXTENSIONS {
        if let Some(stem) = base.strip_suffix(ext) {
            stems.push(stem.to_string());
        }
    }
    if files.contains(&base) {
        return Some(base);
    }
    for stem in &stems {
        for ext in JS_EXTENSIONS {
            let c = format!("{}{}", stem, ext);
            if files.contains(&c) {
                return Some(c);
            }
        }
        for ext in JS_EXTENSIONS {
            let c = format!("{}/index{}", stem, ext);
            if files.contains(&c) {
                return Some(c);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Rust `mod name;`
// ---------------------------------------------------------------------------

/// Directory holding the child modules of `file` (`src/lib.rs` -> `src`, `src/a.rs` -> `src/a`).
fn rust_module_directory(file: &str) -> String {
    let dir = dirname(file);
    let name = file.rsplit('/').next().unwrap_or(file);
    if matches!(name, "mod.rs" | "lib.rs" | "main.rs") {
        return dir.to_string();
    }
    let stem = name.trim_end_matches(".rs");
    if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{}/{}", dir, stem)
    }
}

/// The file declared by `mod <name>;` in `file` (`qualifier` as extracted: the enclosing inline
/// modules as `a/b`, or `path:<file>` for a `#[path]` attribute).
pub(crate) fn rust_mod_file(file: &str, name: &str, qualifier: Option<&str>, files: &HashSet<String>) -> Option<String> {
    let q = qualifier.unwrap_or("");
    if let Some(p) = q.strip_prefix("path:") {
        let c = join_path(dirname(file), p)?;
        return files.contains(&c).then_some(c);
    }
    let mut dirs = vec![rust_module_directory(file)];
    // Crate roots other than lib.rs / main.rs (`src/bin/x.rs`, `tests/x.rs`, ...) keep their
    // modules next to them.
    dirs.push(dirname(file).to_string());
    for dir in dirs {
        let dir = if q.is_empty() {
            dir
        } else if dir.is_empty() {
            q.to_string()
        } else {
            format!("{}/{}", dir, q)
        };
        let prefix = if dir.is_empty() { String::new() } else { format!("{}/", dir) };
        for c in [format!("{}{}.rs", prefix, name), format!("{}{}/mod.rs", prefix, name)] {
            if c != file && files.contains(&c) {
                return Some(c);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Express router mounts
// ---------------------------------------------------------------------------

/// `/api` + `/users` -> `/api/users`.
fn join_route(prefix: &str, path: &str) -> String {
    let mut out = String::new();
    for part in [prefix, path] {
        let p = part.trim().trim_matches('/');
        if !p.is_empty() {
            out.push('/');
            out.push_str(p);
        }
    }
    if out.is_empty() {
        "/".to_string()
    } else {
        out
    }
}

#[derive(Default)]
struct ExpressFile {
    /// Receivers routes are declared on, or that mount other routers.
    receivers: HashSet<String>,
    /// (parent receiver, mounted target, prefix)
    mounts: Vec<(String, String, String)>,
    /// exported name -> receiver
    exports: HashMap<String, String>,
    /// local name -> (module specifier, imported name)
    imports: HashMap<String, (String, String)>,
}

type RouterKey = (String, String);

/// Mounted path prefixes per router, keyed by (file, receiver). Only routers that are mounted
/// somewhere appear; a router nobody mounts keeps its own paths.
#[derive(Debug, Default)]
pub(crate) struct ExpressMounts {
    prefixes: HashMap<RouterKey, Vec<String>>,
}

impl ExpressMounts {
    /// Compose every mount of the corpus. `extractions` yields (file, extraction).
    pub(crate) fn compose<'a>(
        extractions: impl Iterator<Item = (&'a str, &'a ExtractionResult)>,
        files: &HashSet<String>,
    ) -> Self {
        let mut by_file: HashMap<String, ExpressFile> = HashMap::new();
        let mut candidates: Vec<(&'a str, &'a ExtractionResult)> = Vec::new();
        for (file, e) in extractions {
            if e.refs.iter().any(|r| {
                r.kind == LINK_EXPRESS_MOUNT || r.kind == LINK_EXPRESS_EXPORT || r.kind == LINK_EXPRESS_ROUTE
            }) {
                candidates.push((file, e));
            }
        }
        if !candidates.iter().any(|(_, e)| e.refs.iter().any(|r| r.kind == LINK_EXPRESS_MOUNT)) {
            return Self::default();
        }
        for (file, e) in candidates {
            let ef = by_file.entry(file.to_string()).or_default();
            for r in &e.refs {
                match r.kind.as_str() {
                    LINK_EXPRESS_ROUTE => {
                        ef.receivers.insert(r.target_name.clone());
                    }
                    LINK_EXPRESS_MOUNT => {
                        ef.receivers.insert(r.from.clone());
                        ef.mounts.push((r.from.clone(), r.target_name.clone(), r.qualifier.clone().unwrap_or_default()));
                    }
                    LINK_EXPRESS_EXPORT => {
                        ef.receivers.insert(r.target_name.clone());
                        ef.exports.insert(r.from.clone(), r.target_name.clone());
                    }
                    _ => {}
                }
            }
            for imp in &e.imports {
                if !imp.local_name.is_empty() {
                    ef.imports
                        .insert(imp.local_name.clone(), (imp.source_module.clone(), imp.imported_name.clone()));
                }
            }
        }
        // Every receiver resolves to the router that owns it: a local router, or (through
        // imports, `require` and re-exports, any number of files deep) the router of the module
        // it was imported from.
        let resolve = |file: &str, name: &str| resolve_router(&by_file, files, file, name, 0);
        let mut parents: HashMap<RouterKey, Vec<(RouterKey, String)>> = HashMap::new();
        let mut aliases: HashMap<RouterKey, RouterKey> = HashMap::new();
        for (file, ef) in &by_file {
            for receiver in &ef.receivers {
                if let Some(owner) = resolve(file, receiver) {
                    let key = (file.clone(), receiver.clone());
                    if owner != key {
                        aliases.insert(key, owner);
                    }
                }
            }
            for (parent, target, prefix) in &ef.mounts {
                let (Some(parent_key), Some(child)) = (resolve(file, parent), resolve(file, target)) else { continue };
                if child != parent_key {
                    let entry = parents.entry(child).or_default();
                    if !entry.contains(&(parent_key.clone(), prefix.clone())) {
                        entry.push((parent_key, prefix.clone()));
                    }
                }
            }
        }
        let mut prefixes = HashMap::new();
        for key in parents.keys() {
            let mut visiting = HashSet::new();
            let set = mounted_paths(key, &parents, &mut visiting, 0);
            if !set.is_empty() {
                prefixes.insert(key.clone(), set.into_iter().take(MAX_PREFIXES).collect::<Vec<String>>());
            }
        }
        // Routes declared on an imported router carry that router's mounted paths.
        for (alias, owner) in aliases {
            if let Some(p) = prefixes.get(&owner).cloned() {
                prefixes.insert(alias, p);
            }
        }
        Self { prefixes }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// Rewrite the route nodes of `file` declared on mounted routers to their full paths.
    /// `body_hashes` / `minhashes` stay aligned with `extraction.symbols`.
    pub(crate) fn apply(
        &self,
        file: &str,
        extraction: &mut ExtractionResult,
        body_hashes: &mut Vec<String>,
        minhashes: &mut Vec<Option<(String, usize)>>,
    ) {
        if self.prefixes.is_empty() {
            return;
        }
        let receivers: HashMap<String, String> = extraction
            .refs
            .iter()
            .filter(|r| r.kind == LINK_EXPRESS_ROUTE)
            .map(|r| (r.from.clone(), r.target_name.clone()))
            .collect();
        if receivers.is_empty() {
            return;
        }
        let symbol_count = extraction.symbols.len();
        body_hashes.resize(symbol_count, String::new());
        minhashes.resize(symbol_count, None);
        let mut renamed: HashMap<String, Vec<String>> = HashMap::new();
        for i in 0..symbol_count {
            let sym = &extraction.symbols[i];
            if sym.kind != "route" {
                continue;
            }
            let Some(receiver) = receivers.get(&sym.qualified_name) else { continue };
            let Some(paths) = self.prefixes.get(&(file.to_string(), receiver.clone())) else { continue };
            let Some((method, path)) = sym.name.split_once(' ') else { continue };
            let suffix = sym.qualified_name.strip_prefix(sym.name.as_str()).unwrap_or("").to_string();
            let handler = sym
                .signature
                .as_deref()
                .and_then(|s| s.split_once(" -> ").map(|(_, h)| h.to_string()))
                .unwrap_or_default();
            let (method, path) = (method.to_string(), path.to_string());
            let old_qualified = sym.qualified_name.clone();
            let mut new_names = Vec::new();
            for (k, prefix) in paths.iter().enumerate() {
                let name = format!("{} {}", method, join_route(prefix, &path));
                let qualified = format!("{}{}", name, suffix);
                let mut s = if k == 0 {
                    extraction.symbols[i].clone()
                } else {
                    let s = extraction.symbols[i].clone();
                    body_hashes.push(body_hashes[i].clone());
                    minhashes.push(None);
                    s
                };
                s.signature = Some(format!("{} -> {}", name, handler));
                s.name = name;
                s.qualified_name = qualified.clone();
                if k == 0 {
                    extraction.symbols[i] = s;
                } else {
                    extraction.symbols.push(s);
                }
                new_names.push(qualified);
            }
            renamed.insert(old_qualified, new_names);
        }
        if renamed.is_empty() {
            return;
        }
        let mut extra: Vec<ExtractedRef> = Vec::new();
        for r in extraction.refs.iter_mut() {
            if r.from_kind != "route" && r.kind != LINK_EXPRESS_ROUTE {
                continue;
            }
            let Some(names) = renamed.get(&r.from) else { continue };
            for (k, n) in names.iter().enumerate() {
                if k == 0 {
                    continue;
                }
                let mut c = r.clone();
                c.from = n.clone();
                extra.push(c);
            }
            r.from = names[0].clone();
        }
        extraction.refs.extend(extra);
    }
}

/// The router a name of `file` denotes: a local router receiver, or — through `import`,
/// `require('./x')` and re-exports — the router receiver of the module that declares it.
fn resolve_router(
    by_file: &HashMap<String, ExpressFile>,
    files: &HashSet<String>,
    file: &str,
    name: &str,
    depth: usize,
) -> Option<RouterKey> {
    if depth > MAX_MOUNT_DEPTH {
        return None;
    }
    let ef = by_file.get(file)?;
    if let Some(spec) = name.strip_prefix("require:") {
        let g = resolve_relative_js(file, spec, files)?;
        return resolve_export(by_file, files, &g, "default", depth + 1);
    }
    if let Some((spec, imported)) = ef.imports.get(name) {
        let g = resolve_relative_js(file, spec, files)?;
        return resolve_export(by_file, files, &g, imported, depth + 1);
    }
    ef.receivers.contains(name).then(|| (file.to_string(), name.to_string()))
}

/// The router `file` exports as `exported` (`default` / `*` for the module itself).
fn resolve_export(
    by_file: &HashMap<String, ExpressFile>,
    files: &HashSet<String>,
    file: &str,
    exported: &str,
    depth: usize,
) -> Option<RouterKey> {
    let gf = by_file.get(file)?;
    let local = match exported {
        "default" | "*" => gf.exports.get("default")?.clone(),
        name => gf
            .exports
            .get(name)
            .cloned()
            .or_else(|| gf.receivers.contains(name).then(|| name.to_string()))?,
    };
    resolve_router(by_file, files, file, &local, depth)
}

/// Every full prefix of `key` through its chain of parents.
fn mounted_paths(
    key: &RouterKey,
    parents: &HashMap<RouterKey, Vec<(RouterKey, String)>>,
    visiting: &mut HashSet<RouterKey>,
    depth: usize,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(ps) = parents.get(key) else {
        out.insert(String::new());
        return out;
    };
    if depth >= MAX_MOUNT_DEPTH || !visiting.insert(key.clone()) {
        return out;
    }
    for (parent, prefix) in ps {
        for above in mounted_paths(parent, parents, visiting, depth + 1) {
            let joined = join_route(&above, prefix);
            out.insert(if joined == "/" { String::new() } else { joined });
            if out.len() >= MAX_PREFIXES {
                break;
            }
        }
    }
    visiting.remove(key);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rust_mod_files() {
        let f = files(&["src/lib.rs", "src/util.rs", "src/a.rs", "src/a/b.rs", "src/c/mod.rs", "src/x/y.rs"]);
        assert_eq!(rust_mod_file("src/lib.rs", "util", Some(""), &f).as_deref(), Some("src/util.rs"));
        assert_eq!(rust_mod_file("src/a.rs", "b", Some(""), &f).as_deref(), Some("src/a/b.rs"));
        assert_eq!(rust_mod_file("src/lib.rs", "c", None, &f).as_deref(), Some("src/c/mod.rs"));
        assert_eq!(rust_mod_file("src/lib.rs", "y", Some("x"), &f).as_deref(), Some("src/x/y.rs"));
        assert_eq!(rust_mod_file("src/lib.rs", "z", Some("path:x/y.rs"), &f).as_deref(), Some("src/x/y.rs"));
        assert_eq!(rust_mod_file("src/lib.rs", "nope", None, &f), None);
    }

    #[test]
    fn relative_js() {
        let f = files(&["src/routes/users.ts", "src/api/index.js", "src/app.ts"]);
        assert_eq!(resolve_relative_js("src/app.ts", "./routes/users", &f).as_deref(), Some("src/routes/users.ts"));
        assert_eq!(resolve_relative_js("src/app.ts", "./routes/users.js", &f).as_deref(), Some("src/routes/users.ts"));
        assert_eq!(resolve_relative_js("src/app.ts", "./api", &f).as_deref(), Some("src/api/index.js"));
        assert_eq!(resolve_relative_js("src/app.ts", "express", &f), None);
        assert_eq!(resolve_relative_js("src/app.ts", "../../x", &f), None);
    }
}
