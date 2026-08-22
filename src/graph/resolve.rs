//! In-memory symbol index used while building the graph to resolve imports and calls.

use std::collections::{HashMap, HashSet};

use crate::graph::models::{ExtractedCall, ExtractedImport};

mod swift;
pub(crate) mod ts_infer;

#[derive(Debug, Clone)]
pub(crate) struct NodeMeta {
    pub id: String,
    pub kind: String,
    pub name: String,
    /// Rust functions inside inline `mod x { .. }` blocks carry the module path (`x::f`).
    pub qualified_name: String,
    pub file_path: String,
    pub container: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub is_exported: bool,
}

impl NodeMeta {
    fn is_callable(&self) -> bool {
        self.kind == "function" || self.kind == "method"
    }
    fn is_type(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "struct" | "enum" | "class" | "trait" | "interface" | "type_alias"
        )
    }
    fn is_synthetic(&self) -> bool {
        self.kind == "file" || self.kind == "module"
    }
    /// Declarations that lexically own other declarations.
    fn is_scope(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "namespace" | "class" | "struct" | "enum" | "interface" | "trait" | "function" | "method"
                // A module-level binding owns the callbacks of its initializer.
                | "constant" | "variable"
        )
    }
    fn contains(&self, line: usize, col: usize) -> bool {
        let after_start =
            line > self.start_line || (line == self.start_line && col >= self.start_col);
        let before_end = line < self.end_line || (line == self.end_line && col <= self.end_col);
        after_start && before_end
    }
}

/// An import binding after resolution.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub local: String,
    pub imported: String,
    pub is_module: bool,
    /// In-repo symbol or file node the binding refers to.
    pub target: Option<usize>,
    /// In-repo file the binding's module resolved to.
    pub resolved_file: Option<String>,
}

pub(crate) struct ImportResolution {
    pub target: Option<usize>,
    pub resolved_file: Option<String>,
    pub confidence: f64,
    pub method: &'static str,
}

pub(crate) enum CallResolution {
    Edge {
        target: usize,
        kind: &'static str,
        confidence: f64,
        method: &'static str,
    },
    Ambiguous(Vec<usize>),
    Unresolved,
}

#[derive(Default)]
pub(crate) struct SymbolIndex {
    pub metas: Vec<NodeMeta>,
    by_name: HashMap<String, Vec<usize>>,
    by_file: HashMap<String, Vec<usize>>,
    members: HashMap<String, Vec<usize>>,
    file_nodes: HashMap<String, usize>,
    all_files: HashSet<String>,
    /// (crate prefix, module path joined by `::`) -> file
    rust_modules: HashMap<(String, String), String>,
    trait_names: HashSet<String>,
    /// Free functions declared in inline Rust modules, by qualified name (`auth::check`).
    inline_mod_fns: HashMap<String, Vec<usize>>,
    /// impl method idx -> trait method idx
    pub impl_of: HashMap<usize, usize>,
    /// `tsconfig.json` / `jsconfig.json` `baseUrl` + `paths` module resolution.
    ts_configs: crate::graph::tsconfig::TsConfigs,
    /// JS/TS re-exports per file: (exported name, imported name, module specifier);
    /// `export * from` is (`*`, `*`, module).
    reexports: HashMap<String, Vec<(String, String, String)>>,
    /// JS/TS `export default` per file: the declaration name it exports.
    default_exports: HashMap<String, String>,
    /// JS/TS named and default imports per file: local name -> (imported name, module
    /// specifier), so `import x from './m'; export default x;` follows `x` to its declaration.
    js_imports: HashMap<String, HashMap<String, (String, String)>>,
    /// TS/JS type facts and bindings for source-only receiver-type inference.
    ts_types: ts_infer::TsTypes,
}

/// Maximum barrel / re-export hops followed (cycles stop here too).
const MAX_REEXPORT_DEPTH: usize = 8;

const SELF_RECEIVERS: [&str; 4] = ["self", "this", "cls", "Self"];
const JS_EXTS: [&str; 9] = [
    ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts",
];

/// Crate prefix (path before `src/`) and module path segments of a Rust file.
pub(crate) fn rust_crate_and_module(file: &str) -> (String, Vec<String>) {
    let stem = file.strip_suffix(".rs").unwrap_or(file);
    let (prefix, rest) = if let Some(rest) = stem.strip_prefix("src/") {
        (String::new(), rest)
    } else if let Some(pos) = stem.find("/src/") {
        (stem[..pos + 1].to_string(), &stem[pos + 5..])
    } else {
        (String::new(), stem)
    };
    let mut segs: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if matches!(
        segs.last().map(String::as_str),
        Some("mod") | Some("lib") | Some("main")
    ) {
        segs.pop();
    }
    (prefix, segs)
}

fn parent_dir(file: &str) -> &str {
    file.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Join and normalise `.`/`..` components of a `/`-separated relative path.
fn normalize_join(base: &str, rel: &str) -> String {
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

impl SymbolIndex {
    pub fn new(metas: Vec<NodeMeta>, all_files: HashSet<String>) -> Self {
        let mut idx = SymbolIndex {
            metas,
            all_files,
            ..Default::default()
        };
        for (i, m) in idx.metas.iter().enumerate() {
            idx.by_file.entry(m.file_path.clone()).or_default().push(i);
            if m.kind == "file" {
                idx.file_nodes.insert(m.file_path.clone(), i);
                continue;
            }
            idx.by_name.entry(m.name.clone()).or_default().push(i);
            if let Some(c) = &m.container {
                idx.members.entry(c.clone()).or_default().push(i);
            }
            if m.kind == "trait" {
                idx.trait_names.insert(m.name.clone());
            }
            if m.kind == "function" && m.container.is_none() && m.qualified_name.contains("::") {
                idx.inline_mod_fns
                    .entry(m.qualified_name.clone())
                    .or_default()
                    .push(i);
            }
        }
        for f in &idx.all_files {
            if f.ends_with(".rs") {
                let (prefix, segs) = rust_crate_and_module(f);
                idx.rust_modules
                    .entry((prefix, segs.join("::")))
                    .or_insert_with(|| f.clone());
            }
        }
        idx
    }

    pub fn file_node(&self, file: &str) -> Option<usize> {
        self.file_nodes.get(file).copied()
    }

    /// Container node (struct/class/trait...) for a member, preferring the member's own file.
    pub fn container_of(&self, i: usize) -> Option<usize> {
        let m = &self.metas[i];
        let c = m.container.as_ref()?;
        let cands: Vec<usize> = self
            .by_name
            .get(c)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|&j| self.metas[j].is_type())
                    .collect()
            })
            .unwrap_or_default();
        let same_file: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&j| self.metas[j].file_path == m.file_path)
            .collect();
        if same_file.len() == 1 {
            return Some(same_file[0]);
        }
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        None
    }

    /// Innermost function/method containing a position, else the file node.
    pub fn enclosing_callable(&self, file: &str, line: usize, col: usize) -> Option<usize> {
        let mut best: Option<usize> = None;
        for &i in self.by_file.get(file).map(|v| v.as_slice()).unwrap_or(&[]) {
            let m = &self.metas[i];
            if !m.is_callable() || !m.contains(line, col) {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => {
                    let bm = &self.metas[b];
                    (m.end_line - m.start_line) < (bm.end_line - bm.start_line)
                        || ((m.end_line - m.start_line) == (bm.end_line - bm.start_line)
                            && m.start_col >= bm.start_col)
                }
            };
            if better {
                best = Some(i);
            }
        }
        best.or_else(|| self.enclosing_initializer(file, line, col))
            .or_else(|| self.file_node(file))
    }

    /// Innermost field / property / constant whose declaration (initializer included) contains
    /// a position: a call in `private readonly X x = Make();` is made by the field, not the file.
    fn enclosing_initializer(&self, file: &str, line: usize, col: usize) -> Option<usize> {
        let mut best: Option<usize> = None;
        for &i in self.by_file.get(file).map(|v| v.as_slice()).unwrap_or(&[]) {
            let m = &self.metas[i];
            if !matches!(m.kind.as_str(), "field" | "property" | "constant" | "variable") || !m.contains(line, col) {
                continue;
            }
            let size = |m: &NodeMeta| (m.end_line - m.start_line, m.end_col.abs_diff(m.start_col));
            if best.is_none_or(|b| size(m) < size(&self.metas[b])) {
                best = Some(i);
            }
        }
        best
    }

    fn top_level_in_file(&self, file: &str, name: &str) -> Option<usize> {
        let cands: Vec<usize> = self
            .by_file
            .get(file)?
            .iter()
            .copied()
            .filter(|&i| {
                let m = &self.metas[i];
                m.name == name && m.container.is_none() && !m.is_synthetic()
            })
            .collect();
        // Prefer the outermost definition if a name is reused for nested helpers.
        cands.into_iter().min_by_key(|&i| self.metas[i].start_line)
    }

    fn members_named(&self, container: &str, name: &str) -> Vec<usize> {
        self.members
            .get(container)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|&i| self.metas[i].name == name)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Callable members (methods) of a container with a given name.
    fn callable_members(&self, container: &str, name: &str) -> Vec<usize> {
        self.members_named(container, name)
            .into_iter()
            .filter(|&i| self.metas[i].is_callable())
            .collect()
    }

    /// Lexical parent of a declaration: its resolved container (impl/class owner), else the
    /// innermost declaration in the same file whose span strictly encloses it, else the file.
    pub fn lexical_parent(&self, i: usize) -> Option<usize> {
        let m = &self.metas[i];
        if m.is_synthetic() {
            return None;
        }
        // A framework route is a file-level fact that references its handler; it is never a
        // lexical child of the handler (or of the controller) it was declared next to.
        if m.kind == "route" {
            return self.file_node(&m.file_path);
        }
        if let Some(c) = self.container_of(i) {
            return Some(c);
        }
        let mut best: Option<usize> = None;
        for &j in self.by_file.get(&m.file_path).map(|v| v.as_slice()).unwrap_or(&[]) {
            if j == i {
                continue;
            }
            let p = &self.metas[j];
            if !p.is_scope() || p.is_synthetic() {
                continue;
            }
            let encloses_start = p.contains(m.start_line, m.start_col);
            let encloses_end = p.contains(m.end_line, m.end_col);
            let same_span = p.start_line == m.start_line
                && p.end_line == m.end_line
                && p.start_col == m.start_col
                && p.end_col == m.end_col;
            if !(encloses_start && encloses_end) || same_span {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => {
                    let bm = &self.metas[b];
                    (p.end_line - p.start_line, p.end_col.abs_diff(p.start_col))
                        < (bm.end_line - bm.start_line, bm.end_col.abs_diff(bm.start_col))
                }
            };
            if better {
                best = Some(j);
            }
        }
        best.or_else(|| self.file_node(&m.file_path))
    }

    /// (subclass method, base method) pairs with the same name, each side unique in its class.
    pub fn override_pairs(&self, class: &NodeMeta, base: &NodeMeta) -> Vec<(usize, usize)> {
        let in_file = |container: &str, file: &str| -> Vec<usize> {
            self.members
                .get(container)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|&i| self.metas[i].is_callable() && self.metas[i].file_path == file)
                        .collect()
                })
                .unwrap_or_default()
        };
        let base_methods = in_file(&base.name, &base.file_path);
        let mut out = Vec::new();
        for m in in_file(&class.name, &class.file_path) {
            let name = &self.metas[m].name;
            let matches: Vec<usize> = base_methods
                .iter()
                .copied()
                .filter(|&b| &self.metas[b].name == name)
                .collect();
            if matches.len() == 1 {
                out.push((m, matches[0]));
            }
        }
        out
    }

    /// Find a declared symbol in a file by qualified name and (optionally) kind.
    pub fn symbol_in_file(&self, file: &str, qualified_name: &str, kind: &str) -> Option<usize> {
        self.by_file.get(file)?.iter().copied().find(|&i| {
            let m = &self.metas[i];
            m.qualified_name == qualified_name && (kind.is_empty() || m.kind == kind)
        })
    }

    /// Functions/methods of a file whose qualified name (or, failing that, name) is `name`.
    pub fn callables_in_file(&self, file: &str, name: &str) -> Vec<usize> {
        let in_file: Vec<usize> = self
            .by_file
            .get(file)
            .map(|v| v.iter().copied().filter(|&i| self.metas[i].is_callable()).collect())
            .unwrap_or_default();
        let exact: Vec<usize> = in_file
            .iter()
            .copied()
            .filter(|&i| self.metas[i].qualified_name == name)
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        in_file
            .into_iter()
            .filter(|&i| self.metas[i].name == name)
            .collect()
    }

    /// Files this file's import bindings resolved to.
    fn imported_files(bindings: &[Binding]) -> HashSet<&str> {
        bindings
            .iter()
            .filter_map(|b| b.resolved_file.as_deref())
            .collect()
    }

    /// Conservative resolution of a non-call reference (`extends`, `instantiates`, `returns`,
    /// ...). Only lexical scope and explicit import evidence bind a name; repository-wide
    /// uniqueness is never treated as proof.
    pub fn resolve_named(
        &self,
        file: &str,
        name: &str,
        qualifier: Option<&str>,
        kinds: &[&str],
        bindings: &[Binding],
        from: Option<usize>,
    ) -> CallResolution {
        if file.ends_with(".swift") {
            return self.swift_resolve_named(file, name, qualifier, kinds, bindings, from);
        }
        self.resolve_named_generic(file, name, qualifier, kinds, bindings, from)
    }

    fn resolve_named_generic(
        &self,
        file: &str,
        name: &str,
        qualifier: Option<&str>,
        kinds: &[&str],
        bindings: &[Binding],
        from: Option<usize>,
    ) -> CallResolution {
        let kind_ok = |i: usize| {
            let m = &self.metas[i];
            !m.is_synthetic() && (kinds.is_empty() || kinds.contains(&m.kind.as_str()))
        };
        if let Some(q) = qualifier.map(str::trim).filter(|q| !q.is_empty()) {
            let segs: Vec<&str> = q
                .split(['.', ':'])
                .filter(|s| !s.is_empty())
                .collect();
            let first = segs.first().copied().unwrap_or(q);
            let last = segs.last().copied().unwrap_or(q);
            // Module alias (`models.Base`, `ns.Thing`).
            if segs.len() == 1 {
                if let Some(f) = bindings
                    .iter()
                    .find(|b| b.local == first)
                    .and_then(|b| b.resolved_file.as_deref())
                {
                    if let Some(t) = self.top_level_in_file(f, name).filter(|&t| kind_ok(t)) {
                        return Self::edge(t, "", 1.0, "explicit-import");
                    }
                }
            }
            // Rust module path (`crate::models::User`).
            if file.ends_with(".rs") {
                let path: Vec<String> = q
                    .split("::")
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if let Some((prefix, abs)) = self.rust_abs_path(file, &path) {
                    if let Some(f) = self.rust_modules.get(&(prefix, abs.join("::"))) {
                        if let Some(t) = self.top_level_in_file(f, name).filter(|&t| kind_ok(t)) {
                            return Self::edge(t, "", 1.0, "qualifier_module");
                        }
                    }
                }
            }
            // Declaration inside a TypeScript namespace (`NS.Base`).
            if let Some(t) = self.namespace_member(file, q, name, bindings, kind_ok) {
                return Self::edge(t, "", 1.0, "namespace_member");
            }
            // Member of a known type (`Shape::Circle`).
            let type_name = bindings
                .iter()
                .find(|b| b.local == last && !b.is_module)
                .map(|b| b.imported.as_str())
                .unwrap_or(last);
            let members: Vec<usize> = self
                .members_named(type_name, name)
                .into_iter()
                .filter(|&i| kind_ok(i))
                .collect();
            if let Some(res) = self.one_or_ambiguous(members, file, 1.0, "qualifier_type") {
                return res;
            }
            return CallResolution::Unresolved;
        }

        // 1. Same file: the lexical container first, then module level.
        let same_file: Vec<usize> = self
            .by_file
            .get(file)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|&i| self.metas[i].name == name && kind_ok(i) && Some(i) != from)
                    .collect()
            })
            .unwrap_or_default();
        if !same_file.is_empty() {
            let top: Vec<usize> = same_file
                .iter()
                .copied()
                .filter(|&i| self.metas[i].container.is_none())
                .collect();
            return match top.len() {
                1 => Self::edge(top[0], "", 1.0, "lexical-scope"),
                0 if same_file.len() == 1 => Self::edge(same_file[0], "", 0.9, "lexical-scope"),
                0 => CallResolution::Ambiguous(same_file),
                _ => CallResolution::Ambiguous(top),
            };
        }

        // 2. Explicit import binding.
        if let Some(b) = bindings.iter().find(|b| b.local == name) {
            if let Some(t) = b.target.filter(|&t| kind_ok(t)) {
                return Self::edge(t, "", 1.0, "explicit-import");
            }
            if let Some(f) = b.resolved_file.as_deref() {
                let imported = if b.imported == "default" || b.imported.is_empty() {
                    name
                } else {
                    b.imported.as_str()
                };
                if let Some(t) = self.top_level_in_file(f, imported).filter(|&t| kind_ok(t)) {
                    return Self::edge(t, "", 1.0, "explicit-import");
                }
            }
            return CallResolution::Unresolved;
        }

        // 3. A unique candidate inside a file this file imports (glob / namespace imports).
        let imported = Self::imported_files(bindings);
        if !imported.is_empty() {
            let cands: Vec<usize> = self
                .by_name
                .get(name)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|&i| {
                            kind_ok(i)
                                && self.metas[i].container.is_none()
                                && imported.contains(self.metas[i].file_path.as_str())
                        })
                        .collect()
                })
                .unwrap_or_default();
            if cands.len() == 1 {
                return Self::edge(cands[0], "", 0.9, "imported-file");
            }
            if cands.len() > 1 {
                return CallResolution::Ambiguous(cands);
            }
        }

        // 4. Rust: items of the same module tree are visible through `super::`/`crate::` paths
        // that were not imported; that is not proof, so report candidates without an edge.
        let global: Vec<usize> = self
            .by_name
            .get(name)
            .map(|v| v.iter().copied().filter(|&i| kind_ok(i)).collect())
            .unwrap_or_default();
        if global.len() > 1 {
            CallResolution::Ambiguous(global)
        } else {
            CallResolution::Unresolved
        }
    }

    fn prefer_file(&self, cands: Vec<usize>, file: &str) -> Vec<usize> {
        let same: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| self.metas[i].file_path == file)
            .collect();
        if same.is_empty() {
            cands
        } else {
            same
        }
    }

    pub fn is_trait_member(&self, i: usize) -> bool {
        self.metas[i]
            .container
            .as_ref()
            .map(|c| self.trait_names.contains(c))
            .unwrap_or(false)
    }

    // ------------------------------------------------------------------
    // Trait implementations
    // ------------------------------------------------------------------

    /// Returns (type idx, trait idx) for `impl Trait for Type` and records `impl_of` pairs.
    pub fn link_trait_impl(
        &mut self,
        file: &str,
        trait_name: &str,
        type_name: &str,
    ) -> (Option<usize>, Option<usize>) {
        let trait_base = trait_name
            .rsplit("::")
            .next()
            .unwrap_or(trait_name)
            .to_string();
        let type_base = type_name
            .rsplit("::")
            .next()
            .unwrap_or(type_name)
            .to_string();

        let pick = |kinds: &[&str], name: &str| -> Option<usize> {
            let cands: Vec<usize> = self
                .by_name
                .get(name)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|&i| kinds.contains(&self.metas[i].kind.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            let cands = self.prefer_file(cands, file);
            if cands.len() == 1 {
                Some(cands[0])
            } else {
                None
            }
        };
        let trait_idx = pick(&["trait"], &trait_base);
        let type_idx = pick(&["struct", "enum", "class"], &type_base);

        if let Some(t) = trait_idx {
            let trait_file = self.metas[t].file_path.clone();
            let trait_methods: Vec<usize> = self
                .members
                .get(&trait_base)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|&i| self.metas[i].file_path == trait_file)
                        .collect()
                })
                .unwrap_or_default();
            for tm in trait_methods {
                let name = self.metas[tm].name.clone();
                let impls = self.prefer_file(self.members_named(&type_base, &name), file);
                if impls.len() == 1 && impls[0] != tm {
                    self.impl_of.insert(impls[0], tm);
                }
            }
        }
        (type_idx, trait_idx)
    }

    // ------------------------------------------------------------------
    // Imports
    // ------------------------------------------------------------------

    pub fn resolve_import(
        &self,
        file: &str,
        language: &str,
        imp: &ExtractedImport,
    ) -> ImportResolution {
        match language {
            "rust" => self.resolve_rust_import(file, imp),
            "typescript" | "tsx" | "javascript" => self.resolve_js_import(file, imp),
            "python" => self.resolve_py_import(file, imp),
            _ => ImportResolution {
                target: None,
                resolved_file: None,
                confidence: 0.0,
                method: "unsupported",
            },
        }
    }

    fn unresolved_import() -> ImportResolution {
        ImportResolution {
            target: None,
            resolved_file: None,
            confidence: 0.0,
            method: "external",
        }
    }

    /// Absolute crate-relative module path for a Rust `use` path, or None for external crates.
    fn rust_abs_path(&self, file: &str, path: &[String]) -> Option<(String, Vec<String>)> {
        let (prefix, self_mod) = rust_crate_and_module(file);
        let head = path.first()?.as_str();
        let abs = match head {
            "crate" => path[1..].to_vec(),
            "self" => {
                let mut v = self_mod.clone();
                v.extend_from_slice(&path[1..]);
                v
            }
            "super" => {
                let supers = path.iter().take_while(|s| s.as_str() == "super").count();
                let keep = self_mod.len().saturating_sub(supers);
                let mut v = self_mod[..keep].to_vec();
                v.extend_from_slice(&path[supers..]);
                v
            }
            first => {
                if self
                    .rust_modules
                    .contains_key(&(prefix.clone(), first.to_string()))
                {
                    path.to_vec()
                } else {
                    return None;
                }
            }
        };
        Some((prefix, abs))
    }

    fn resolve_rust_import(&self, file: &str, imp: &ExtractedImport) -> ImportResolution {
        let mut path: Vec<String> = imp
            .source_module
            .split("::")
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let wildcard = imp.imported_name == "*";
        let bare_module = imp.is_module && path.len() == 1 && path[0] == imp.imported_name;
        if !(wildcard || bare_module) {
            path.push(imp.imported_name.clone());
        }

        if let Some((prefix, abs)) = self.rust_abs_path(file, &path) {
            let module_file = |segs: &[String]| {
                self.rust_modules
                    .get(&(prefix.clone(), segs.join("::")))
                    .cloned()
            };
            if wildcard {
                if let Some(f) = module_file(&abs) {
                    return ImportResolution {
                        target: self.file_node(&f),
                        resolved_file: Some(f),
                        confidence: 1.0,
                        method: "rust_module_path",
                    };
                }
                return Self::unresolved_import();
            }
            if let Some(item) = abs.last() {
                let m = &abs[..abs.len() - 1];
                if let Some(f) = module_file(m) {
                    // `use crate::graph;` names the module: a `mod graph;` declaration in the
                    // parent file stands for the module file, which binds instead.
                    let declared_module = |t: usize| {
                        self.metas[t].kind == "namespace" && module_file(&abs).is_some()
                    };
                    if let Some(t) = self.top_level_in_file(&f, item).filter(|&t| !declared_module(t)) {
                        return ImportResolution {
                            target: Some(t),
                            resolved_file: Some(f),
                            confidence: 1.0,
                            method: "rust_module_path",
                        };
                    }
                }
                if let Some(f) = module_file(&abs) {
                    return ImportResolution {
                        target: self.file_node(&f),
                        resolved_file: Some(f),
                        confidence: 1.0,
                        method: "rust_module_path",
                    };
                }
                // Re-export: unique top-level item with that name below the module.
                let cands: Vec<usize> = self
                    .by_name
                    .get(item)
                    .map(|v| {
                        v.iter()
                            .copied()
                            .filter(|&i| {
                                let meta = &self.metas[i];
                                if meta.container.is_some()
                                    || meta.is_synthetic()
                                    || !meta.file_path.ends_with(".rs")
                                {
                                    return false;
                                }
                                let (p, segs) = rust_crate_and_module(&meta.file_path);
                                p == prefix && segs.starts_with(m)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if cands.len() == 1 {
                    let t = cands[0];
                    return ImportResolution {
                        target: Some(t),
                        resolved_file: Some(self.metas[t].file_path.clone()),
                        confidence: 0.8,
                        method: "rust_reexport",
                    };
                }
            }
            return Self::unresolved_import();
        }

        // External crate path. Workspace crates are indexed too: accept a unique top-level match.
        let head = path.first().map(String::as_str).unwrap_or("");
        if wildcard || matches!(head, "std" | "core" | "alloc") {
            return Self::unresolved_import();
        }
        let item = path.last().cloned().unwrap_or_default();
        let cands: Vec<usize> = self
            .by_name
            .get(&item)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|&i| {
                        let m = &self.metas[i];
                        m.container.is_none() && !m.is_synthetic() && m.file_path.ends_with(".rs")
                    })
                    .collect()
            })
            .unwrap_or_default();
        if cands.len() == 1 {
            // Workspace-crate guess by name only: low confidence, marked heuristic.
            let t = cands[0];
            return ImportResolution {
                target: Some(t),
                resolved_file: Some(self.metas[t].file_path.clone()),
                confidence: 0.5,
                method: "global_unique",
            };
        }
        Self::unresolved_import()
    }

    /// Install the project's tsconfig/jsconfig module resolution.
    pub fn set_ts_configs(&mut self, configs: crate::graph::tsconfig::TsConfigs) {
        self.ts_configs = configs;
    }

    /// Record a JS/TS re-export (`export { a as b } from './m'`, `export * from './m'`).
    pub fn add_reexport(&mut self, file: &str, imp: &ExtractedImport) {
        let exported = if imp.imported_name == "*" && imp.local_name.is_empty() {
            "*".to_string()
        } else {
            imp.local_name.clone()
        };
        self.reexports.entry(file.to_string()).or_default().push((
            exported,
            imp.imported_name.clone(),
            imp.source_module.clone(),
        ));
    }

    /// Record a JS/TS file's `export default` declaration name.
    pub fn set_default_export(&mut self, file: &str, name: &str) {
        self.default_exports.insert(file.to_string(), name.to_string());
    }

    /// Record a JS/TS file's (non re-export) named or default import binding.
    pub fn add_js_import(&mut self, file: &str, imp: &ExtractedImport) {
        if imp.is_reexport || imp.is_module || imp.local_name.is_empty() || imp.imported_name == "*" {
            return;
        }
        self.js_imports.entry(file.to_string()).or_default().insert(
            imp.local_name.clone(),
            (imp.imported_name.clone(), imp.source_module.clone()),
        );
    }

    /// The declaration a JS/TS file exports as `default`, following barrels
    /// (`export { default } from './m'`) and re-exported imports
    /// (`import x from './m'; export default x;`).
    fn default_export_of(&self, file: &str) -> Option<usize> {
        self.default_export_at(file, 0)
    }

    fn default_export_at(&self, file: &str, depth: usize) -> Option<usize> {
        if depth > MAX_REEXPORT_DEPTH {
            return None;
        }
        if let Some(name) = self.default_exports.get(file) {
            if let Some(t) = self.top_level_in_file(file, name) {
                return Some(t);
            }
            if let Some(t) = self.imported_declaration(file, name, depth) {
                return Some(t);
            }
        }
        self.follow_reexport(file, "default", depth + 1)
    }

    /// The declaration a file's import binding `local` names, through barrels.
    fn imported_declaration(&self, file: &str, local: &str, depth: usize) -> Option<usize> {
        let (imported, module) = self.js_imports.get(file)?.get(local)?;
        let target_file = self.js_module_file(file, module)?;
        if imported == "default" {
            return self.default_export_at(&target_file, depth + 1);
        }
        self.top_level_in_file(&target_file, imported)
            .or_else(|| self.follow_reexport(&target_file, imported, depth + 1))
    }

    /// A declaration qualified by a TypeScript namespace path (`NS.inner`, `A.B.f`): in the same
    /// file, else in the file the path's first segment was imported from.
    fn namespace_member(
        &self,
        file: &str,
        path: &str,
        name: &str,
        bindings: &[Binding],
        kind_ok: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        if ![".ts", ".tsx", ".mts", ".cts"].iter().any(|e| file.ends_with(e)) {
            return None;
        }
        let first = path.split('.').next().unwrap_or(path);
        let wanted = |f: &str, qualified: &str| -> Vec<usize> {
            self.by_name
                .get(name)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|&i| {
                            let m = &self.metas[i];
                            m.file_path == f
                                && m.container.is_none()
                                && m.qualified_name == qualified
                                && kind_ok(i)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let same = wanted(file, &format!("{}.{}", path, name));
        if same.len() == 1 {
            return Some(same[0]);
        }
        // `import { NS } from './ns'` (possibly aliased): the namespace's own name qualifies.
        let b = bindings.iter().find(|b| b.local == first && !b.is_module)?;
        let f = b.resolved_file.as_deref()?;
        let imported = if b.imported.is_empty() || b.imported == "default" {
            first
        } else {
            b.imported.as_str()
        };
        let rest = &path[first.len()..];
        let there = wanted(f, &format!("{}{}.{}", imported, rest, name));
        (there.len() == 1).then(|| there[0])
    }

    /// Follow re-export chains from `file` to the declaration exported as `name`.
    fn follow_reexport(&self, file: &str, name: &str, depth: usize) -> Option<usize> {
        if depth > MAX_REEXPORT_DEPTH {
            return None;
        }
        let list = self.reexports.get(file)?;
        // Named re-exports first, then `export *`.
        for (exported, imported, module) in list {
            if exported != name {
                continue;
            }
            let target_file = self.js_module_file(file, module)?;
            if imported == "*" {
                return self.file_node(&target_file);
            }
            // `export { default as X } from './m'`: the declaration `m` exports as default.
            if imported == "default" {
                return self.default_export_at(&target_file, depth + 1);
            }
            return self
                .top_level_in_file(&target_file, imported)
                .or_else(|| self.follow_reexport(&target_file, imported, depth + 1));
        }
        for (exported, _, module) in list {
            if exported != "*" {
                continue;
            }
            if let Some(target_file) = self.js_module_file(file, module) {
                if let Some(t) = self
                    .top_level_in_file(&target_file, name)
                    .filter(|&t| self.metas[t].is_exported)
                    .or_else(|| self.follow_reexport(&target_file, name, depth + 1))
                {
                    return Some(t);
                }
            }
        }
        None
    }

    fn js_module_file(&self, file: &str, spec: &str) -> Option<String> {
        if !(spec.starts_with('.') || spec.starts_with('/')) {
            for base in self.ts_configs.candidates(file, spec) {
                if let Some(f) = self.js_file_for_base(&base) {
                    return Some(f);
                }
            }
            return None;
        }
        let joined = normalize_join(parent_dir(file), spec);
        self.js_file_for_base(&joined)
    }

    /// The corpus file a module path (with or without extension) names.
    fn js_file_for_base(&self, joined: &str) -> Option<String> {
        let joined = joined.to_string();
        let mut candidates = vec![joined.clone()];
        for ext in JS_EXTS {
            candidates.push(format!("{}{}", joined, ext));
        }
        for ext in JS_EXTS {
            candidates.push(format!("{}/index{}", joined, ext));
        }
        // TS ESM convention: `import './x.js'` refers to `x.ts`.
        if let Some(stem) = joined.strip_suffix(".js") {
            candidates.push(format!("{}.ts", stem));
            candidates.push(format!("{}.tsx", stem));
        }
        candidates.into_iter().find(|c| self.all_files.contains(c))
    }

    fn resolve_js_import(&self, file: &str, imp: &ExtractedImport) -> ImportResolution {
        let target_file = match self.js_module_file(file, &imp.source_module) {
            Some(f) => f,
            None => return Self::unresolved_import(),
        };
        let mut via_reexport = false;
        let target = if imp.is_module
            || imp.imported_name.is_empty()
            || imp.imported_name == "*"
            || imp.imported_name == "default"
        {
            self.file_node(&target_file)
        } else {
            self.top_level_in_file(&target_file, &imp.imported_name)
                .or_else(|| {
                    // Barrel files: follow `export { x } from` / `export * from` chains.
                    let t = self.follow_reexport(&target_file, &imp.imported_name, 0)?;
                    via_reexport = true;
                    Some(t)
                })
                .or_else(|| self.file_node(&target_file))
        };
        let relative = imp.source_module.starts_with('.') || imp.source_module.starts_with('/');
        ImportResolution {
            target,
            resolved_file: Some(target_file),
            confidence: 1.0,
            method: match (via_reexport, relative) {
                (true, _) => "reexport_chain",
                (false, true) => "relative_path",
                (false, false) => "tsconfig_paths",
            },
        }
    }

    fn py_module_file(&self, file: &str, spec: &str) -> Option<String> {
        let dots = spec.chars().take_while(|c| *c == '.').count();
        let rest: Vec<&str> = spec[dots..].split('.').filter(|s| !s.is_empty()).collect();
        if dots > 0 {
            let mut base = parent_dir(file).to_string();
            for _ in 1..dots {
                base = parent_dir(&base).to_string();
            }
            let joined = if rest.is_empty() {
                base.clone()
            } else {
                normalize_join(&base, &rest.join("/"))
            };
            let cands = [format!("{}.py", joined), format!("{}/__init__.py", joined)];
            return cands.into_iter().find(|c| self.all_files.contains(c));
        }
        if rest.is_empty() {
            return None;
        }
        let rel = rest.join("/");
        let suffixes = [format!("{}.py", rel), format!("{}/__init__.py", rel)];
        let mut matches: Vec<&String> = self
            .all_files
            .iter()
            .filter(|f| {
                suffixes
                    .iter()
                    .any(|s| f.as_str() == s.as_str() || f.ends_with(&format!("/{}", s)))
            })
            .collect();
        matches.sort_by_key(|f| (f.len(), f.to_string()));
        matches.first().map(|s| s.to_string())
    }

    fn resolve_py_import(&self, file: &str, imp: &ExtractedImport) -> ImportResolution {
        let spec = &imp.source_module;
        if imp.is_module {
            return match self.py_module_file(file, spec) {
                Some(f) => ImportResolution {
                    target: self.file_node(&f),
                    resolved_file: Some(f),
                    confidence: 1.0,
                    method: "python_module_path",
                },
                None => Self::unresolved_import(),
            };
        }
        let module_file = self.py_module_file(file, spec);
        if let Some(f) = &module_file {
            if let Some(t) = self.top_level_in_file(f, &imp.imported_name) {
                return ImportResolution {
                    target: Some(t),
                    resolved_file: Some(f.clone()),
                    confidence: 1.0,
                    method: "python_module_path",
                };
            }
        }
        let sub_spec = if spec.ends_with('.') {
            format!("{}{}", spec, imp.imported_name)
        } else {
            format!("{}.{}", spec, imp.imported_name)
        };
        if let Some(f) = self.py_module_file(file, &sub_spec) {
            return ImportResolution {
                target: self.file_node(&f),
                resolved_file: Some(f),
                confidence: 1.0,
                method: "python_submodule",
            };
        }
        match module_file {
            Some(f) => ImportResolution {
                target: self.file_node(&f),
                resolved_file: Some(f),
                confidence: 0.7,
                method: "python_module_path",
            },
            None => Self::unresolved_import(),
        }
    }

    // ------------------------------------------------------------------
    // Calls
    // ------------------------------------------------------------------

    fn edge(
        target: usize,
        kind: &'static str,
        confidence: f64,
        method: &'static str,
    ) -> CallResolution {
        CallResolution::Edge {
            target,
            kind,
            confidence,
            method,
        }
    }

    fn one_or_ambiguous(
        &self,
        cands: Vec<usize>,
        file: &str,
        conf: f64,
        method: &'static str,
    ) -> Option<CallResolution> {
        let cands = self.prefer_file(cands, file);
        match cands.len() {
            0 => None,
            1 => Some(Self::edge(cands[0], "calls", conf, method)),
            _ => Some(CallResolution::Ambiguous(cands)),
        }
    }

    /// Resolve a call in priority order: same container (self/this/Self), qualifier path,
    /// same file, imported bindings, then a unique global match. Ambiguity is reported, never
    /// fanned out into edges to every same-named symbol.
    pub fn resolve_call(
        &self,
        file: &str,
        call: &ExtractedCall,
        caller: Option<usize>,
        bindings: &[Binding],
    ) -> CallResolution {
        if file.ends_with(".swift") {
            return self.swift_resolve_call(file, call, caller, bindings);
        }
        self.resolve_call_generic(file, call, caller, bindings)
    }

    fn resolve_call_generic(
        &self,
        file: &str,
        call: &ExtractedCall,
        caller: Option<usize>,
        bindings: &[Binding],
    ) -> CallResolution {
        let name = call.target_name.as_str();
        let caller_container = caller.and_then(|c| self.metas[c].container.clone());
        let recv = call.receiver.as_deref().map(str::trim);
        let qualifier = call.qualifier.as_deref();
        let is_self =
            recv.map(|r| SELF_RECEIVERS.contains(&r)).unwrap_or(false) || qualifier == Some("Self");

        // 1. Same container.
        if is_self {
            if let Some(c) = &caller_container {
                if let Some(res) =
                    self.one_or_ambiguous(self.callable_members(c, name), file, 1.0, "same_container")
                {
                    return res;
                }
            }
        }

        // 1b. TS/JS member call whose receiver type the extractor could describe.
        if let Some(res) = self.resolve_ts_inferred(file, call) {
            return res;
        }

        // 2. Qualifier path (`Type::assoc()`, `module::func()`).
        if let Some(q) = qualifier.filter(|q| *q != "Self") {
            let segs: Vec<&str> = q.split("::").filter(|s| !s.is_empty()).collect();
            let last = segs.last().copied().unwrap_or(q);
            let type_name = bindings
                .iter()
                .find(|b| b.local == last && !b.is_module)
                .map(|b| b.imported.as_str())
                .unwrap_or(last);
            if let Some(res) = self.one_or_ambiguous(
                self.callable_members(type_name, name),
                file,
                1.0,
                "qualifier_type",
            ) {
                return res;
            }
            // Module path: a bound module alias or a crate-relative module.
            if segs.len() == 1 {
                if let Some(f) = bindings
                    .iter()
                    .find(|b| b.local == last)
                    .and_then(|b| b.resolved_file.clone())
                {
                    if let Some(t) = self.top_level_in_file(&f, name) {
                        return Self::edge(t, "calls", 1.0, "qualifier_module");
                    }
                }
            }
            if file.ends_with(".rs") {
                let path: Vec<String> = segs.iter().map(|s| s.to_string()).collect();
                if let Some((prefix, abs)) = self.rust_abs_path(file, &path) {
                    if let Some(f) = self.rust_modules.get(&(prefix, abs.join("::"))) {
                        if let Some(t) = self.top_level_in_file(f, name) {
                            return Self::edge(t, "calls", 1.0, "qualifier_module");
                        }
                    }
                }
            }
            // Inline module (`mod auth { pub fn check() }` called as `auth::check()`): match the
            // module-qualified name, ignoring leading `crate`/`self`/`super` segments.
            if file.ends_with(".rs") {
                let mod_segs: Vec<&str> = segs
                    .iter()
                    .copied()
                    .skip_while(|s| matches!(*s, "crate" | "self" | "super"))
                    .collect();
                if !mod_segs.is_empty() {
                    let key = format!("{}::{}", mod_segs.join("::"), name);
                    if let Some(cands) = self.inline_mod_fns.get(&key) {
                        if let Some(res) = self.one_or_ambiguous(
                            self.prefer_file(cands.clone(), file),
                            file,
                            1.0,
                            "qualifier_inline_module",
                        ) {
                            return res;
                        }
                    }
                }
            }
            // Unknown qualifier (external type/module): never guess project-wide.
            return CallResolution::Unresolved;
        }

        if !call.is_method {
            // 3. Same file.
            if let Some(t) = self.top_level_in_file(file, name) {
                let k = &self.metas[t].kind;
                if k == "function" || self.metas[t].is_type() {
                    return Self::edge(t, "calls", 1.0, "same_file");
                }
            }
            // 4. Imported binding.
            if let Some(b) = bindings.iter().find(|b| b.local == name) {
                if let Some(t) = b.target.filter(|&t| !self.metas[t].is_synthetic()) {
                    return Self::edge(t, "calls", 1.0, "import");
                }
                // Default import (`import Thing from './thing'`): bound to the module file;
                // the default export is usually declared under the same name.
                if b.imported == "default" {
                    if let Some(t) = b
                        .resolved_file
                        .as_deref()
                        .and_then(|f| self.default_export_of(f))
                        .filter(|&t| self.metas[t].is_callable() || self.metas[t].is_type())
                    {
                        return Self::edge(t, "calls", 1.0, "default-export");
                    }
                    if let Some(t) = b
                        .resolved_file
                        .as_deref()
                        .and_then(|f| self.top_level_in_file(f, name))
                    {
                        return Self::edge(t, "calls", 0.9, "import_default");
                    }
                }
                // Imported from somewhere we cannot see: do not guess.
                return CallResolution::Unresolved;
            }
            // 5. Free functions with that name. A unique one inside a file this file imports
            // (glob / namespace import) is import evidence; repository-wide uniqueness is not
            // proof, so it only yields a low-confidence `possible_call` marked heuristic.
            let cands: Vec<usize> = self
                .by_name
                .get(name)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|&i| {
                            self.metas[i].kind == "function" && self.metas[i].container.is_none()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let imported = Self::imported_files(bindings);
            let via_import: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|&i| imported.contains(self.metas[i].file_path.as_str()))
                .collect();
            if via_import.len() == 1 {
                return Self::edge(via_import[0], "calls", 0.9, "imported-file");
            }
            return match cands.len() {
                0 => CallResolution::Unresolved,
                1 => Self::edge(cands[0], "possible_call", 0.5, "global_unique"),
                _ => CallResolution::Ambiguous(cands),
            };
        }

        // Method call on a non-self receiver.
        if let Some(r) = recv {
            // Namespace / module object (`ns.fn()`, `module.fn()`).
            if let Some(b) = bindings.iter().find(|b| b.local == r) {
                let binds_file = b
                    .target
                    .map(|t| self.metas[t].kind == "file")
                    .unwrap_or(false);
                if b.is_module || binds_file {
                    if let Some(f) = &b.resolved_file {
                        if let Some(t) = self.top_level_in_file(f, name) {
                            return Self::edge(t, "calls", 1.0, "import_namespace");
                        }
                    }
                    return CallResolution::Unresolved;
                }
                // `ImportedClass.staticMethod()`
                if let Some(res) = self.one_or_ambiguous(
                    self.callable_members(&b.imported, name),
                    file,
                    1.0,
                    "qualifier_type",
                ) {
                    return res;
                }
            }
            // `NS.inner()` / `A.B.f()`: a function declared inside a TypeScript namespace.
            if let Some(t) =
                self.namespace_member(file, r, name, bindings, |i| self.metas[i].is_callable())
            {
                return Self::edge(t, "calls", 1.0, "namespace_member");
            }
            // `const r: Runner = ..; r.run()`: the receiver's explicit type annotation names a
            // project type (possibly imported under an alias) with exactly that member.
            if let Some(t) = call.receiver_type.as_deref().filter(|t| !ts_infer::is_encoded(t)) {
                let type_name = bindings
                    .iter()
                    .find(|b| b.local == t && !b.is_module && !b.imported.is_empty() && b.imported != "default")
                    .map(|b| b.imported.as_str())
                    .unwrap_or(t);
                if let Some(res @ CallResolution::Edge { .. }) =
                    self.one_or_ambiguous(self.callable_members(type_name, name), file, 1.0, "type_annotation")
                {
                    return res;
                }
            }
            // `ClassName.staticMethod()` for a type defined in the project.
            if r.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                && r.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                if let Some(res) =
                    self.one_or_ambiguous(self.callable_members(r, name), file, 1.0, "qualifier_type")
                {
                    return res;
                }
            }
        }

        // Receiver type unknown: only link when every candidate collapses to one trait method,
        // or there is exactly one method with that name in the project.
        let cands: Vec<usize> = self
            .by_name
            .get(name)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|&i| self.metas[i].kind == "method")
                    .collect()
            })
            .unwrap_or_default();
        if cands.is_empty() {
            return CallResolution::Unresolved;
        }
        let mut canonical: Vec<usize> = cands
            .iter()
            .map(|i| *self.impl_of.get(i).unwrap_or(i))
            .collect();
        canonical.sort_unstable();
        canonical.dedup();
        if canonical.len() == 1 {
            let t = canonical[0];
            if self.is_trait_member(t) {
                return Self::edge(t, "calls_trait_method", 0.7, "trait_dispatch");
            }
            return Self::edge(t, "possible_call", 0.5, "global_unique_method");
        }
        CallResolution::Ambiguous(cands)
    }
}
