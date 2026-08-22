use crate::graph::models::{
    ExtractedCall, ExtractedImport, ExtractedRef, ExtractedSymbol, ExtractedTraitImpl,
};
use serde::{Deserialize, Serialize};
use tree_sitter::{Node as TsNode, Parser};

/// Version of the extraction output. Bump whenever extractors change what they emit so cached
/// extractions and existing graphs are recognised as produced by an older extractor.
pub const EXTRACTOR_VERSION: &str = "knobyte-extract-6";

/// Reference kinds that carry cross-file structure facts rather than references: they are
/// linked by the build (see `graph::links`) and never stored as unresolved references.
pub(crate) const LINK_PREFIX: &str = "link:";
/// `mod name;` -> the module file (`qualifier`: enclosing inline modules as `a/b`, or
/// `path:<file>` for a `#[path = ".."]` attribute).
pub(crate) const LINK_RUST_MOD: &str = "link:rust_mod";
/// `parent.use('/prefix', router)` (`from`: mounting receiver, `target_name`: mounted
/// identifier, `qualifier`: prefix).
pub(crate) const LINK_EXPRESS_MOUNT: &str = "link:express_mount";
/// A route declared on a receiver (`from`: route qualified name, `target_name`: receiver).
pub(crate) const LINK_EXPRESS_ROUTE: &str = "link:express_route";
/// A router receiver exported by the file (`from`: `default` or the exported name,
/// `target_name`: receiver).
pub(crate) const LINK_EXPRESS_EXPORT: &str = "link:express_export";

mod csharp;
mod swift;
mod frameworks;
mod typescript;

use typescript::{extract_ts_like, TsLang};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub language: String,
    pub symbols: Vec<ExtractedSymbol>,
    pub calls: Vec<ExtractedCall>,
    pub imports: Vec<ExtractedImport>,
    pub trait_impls: Vec<ExtractedTraitImpl>,
    /// Non-call references (extends, implements, instantiates, returns, type_of, decorates,
    /// references).
    #[serde(default)]
    pub refs: Vec<ExtractedRef>,
    /// `ok`, or `partial` when the parser recovered from syntax errors.
    #[serde(default = "default_parse_status")]
    pub parse_status: String,
}

fn default_parse_status() -> String {
    "ok".to_string()
}

pub fn is_supported_path(path: &std::path::Path) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "md" {
        let path_lower = path.to_string_lossy().to_lowercase();
        return path_lower.contains("adr/")
            || path_lower.contains("adrs/")
            || path_lower.contains("docs/adr");
    }

    matches!(
        ext.as_str(),
        "rs" | "ts"
            | "mts"
            | "cts"
            | "tsx"
            | "js"
            | "mjs"
            | "cjs"
            | "jsx"
            | "py"
            | "cs"
            | "swift"
            | "sql"
            | "json"
            | "yaml"
            | "yml"
    )
}

/// Language label for a path, based on its extension (used for files that yield no symbols).
pub fn language_for_path(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "py" => "python",
        "cs" => "csharp",
        "swift" => "swift",
        "sql" => "sql",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        _ => "unknown",
    }
}

/// True for languages parsed with tree-sitter into functions/classes/imports.
pub fn is_code_language(language: &str) -> bool {
    matches!(
        language,
        "rust" | "typescript" | "tsx" | "javascript" | "python" | "csharp" | "swift"
    )
}

pub fn extract_file(path: &str, content: &str) -> Option<ExtractionResult> {
    let path_obj = std::path::Path::new(path);
    if !is_supported_path(path_obj) {
        return None;
    }

    let ext = path_obj
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "md" {
        return extract_markdown_adr(path, content);
    }

    let mut result = match ext.as_str() {
        "rs" => extract_rust(content),
        "ts" | "mts" | "cts" => extract_ts_like(content, TsLang::TypeScript),
        "tsx" => extract_ts_like(content, TsLang::Tsx),
        "js" | "mjs" | "cjs" | "jsx" => extract_ts_like(content, TsLang::JavaScript),
        "py" => extract_python(content),
        "cs" => csharp::extract_csharp(content),
        "swift" => swift::extract_swift(content),
        "sql" => extract_sql(content),
        "json" | "yaml" | "yml" => extract_json_or_yaml(path, content),
        _ => None,
    }?;

    assign_callers(&result.symbols, &mut result.calls);
    if matches!(ext.as_str(), "ts" | "mts" | "cts" | "tsx" | "js" | "mjs" | "cjs" | "jsx") {
        add_nextjs_routes(path, &mut result);
    }
    if matches!(ext.as_str(), "ts" | "mts" | "cts" | "tsx" | "js" | "mjs" | "cjs" | "jsx" | "py") {
        frameworks::add_framework_routes(path, content, &mut result);
    }
    Some(result)
}

const HTTP_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"];

/// URL path served by a Next.js App Router `route.*` file: `app/api/users/route.ts` ->
/// `/api/users`. Route groups `(x)` are dropped; private folders `_x` opt out of routing.
pub(crate) fn nextjs_route_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
    let stem = file.split('.').next().unwrap_or("");
    if stem != "route" {
        return None;
    }
    let segs: Vec<&str> = dir.split('/').collect();
    let start = segs
        .windows(2)
        .position(|w| w == ["src", "app"])
        .map(|i| i + 2)
        .or_else(|| segs.iter().position(|s| *s == "app").map(|i| i + 1))?;
    let rest = &segs[start..];
    if rest.iter().any(|s| s.starts_with('_')) {
        return None;
    }
    let kept: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|s| !s.is_empty() && !s.starts_with('('))
        .collect();
    Some(format!("/{}", kept.join("/")))
}

/// Next.js route handlers: one `route` node per exported HTTP-verb handler, referencing it.
fn add_nextjs_routes(path: &str, result: &mut ExtractionResult) {
    let Some(route_path) = nextjs_route_path(path) else {
        return;
    };
    let mut routes = Vec::new();
    for sym in &result.symbols {
        if sym.kind != "function" || !sym.is_exported || sym.container.is_some() {
            continue;
        }
        if !HTTP_METHODS.contains(&sym.name.as_str()) || routes.iter().any(|r: &ExtractedSymbol| r.signature.as_deref().is_some_and(|s| s.ends_with(&format!("-> {}", sym.name)))) {
            continue;
        }
        let name = format!("{} {}", sym.name, route_path);
        let mut route = ExtractedSymbol::simple("route", &name, &name, sym.start_line, String::new());
        route.signature = Some(format!("{} -> {}", name, sym.name));
        route.body = route.signature.clone().unwrap_or_default();
        route.start_col = sym.start_col;
        route.end_col = sym.start_col;
        route.visibility = Some("public".to_string());
        // A route is not a module export (no `exports` edge).
        route.is_exported = false;
        result.refs.push(ExtractedRef {
            kind: "references".to_string(),
            from: name.clone(),
            from_kind: "route".to_string(),
            target_name: sym.name.clone(),
            qualifier: Some("framework:nextjs".to_string()),
            line: sym.start_line,
            col: sym.start_col,
        });
        routes.push(route);
    }
    result.symbols.extend(routes);
}

/// Set `caller_name` of every call to the innermost enclosing function/method (span
/// containment); a call outside any callable but inside a field / property / constant
/// declaration (its initializer) is made by that declaration, not the file.
fn assign_callers(symbols: &[ExtractedSymbol], calls: &mut [ExtractedCall]) {
    let innermost = |kinds: &[&str], line: usize, col: usize| -> Option<&ExtractedSymbol> {
        let mut best: Option<&ExtractedSymbol> = None;
        for s in symbols {
            if !kinds.contains(&s.kind.as_str()) || !span_contains(s, line, col) {
                continue;
            }
            if best.is_none_or(|b| span_size(s) < span_size(b)) {
                best = Some(s);
            }
        }
        best
    };
    for call in calls.iter_mut() {
        let best = innermost(&["function", "method"], call.line, call.col)
            .or_else(|| innermost(&["field", "property", "constant", "variable"], call.line, call.col));
        call.caller_name = best.map(|s| s.qualified_name.clone()).unwrap_or_default();
    }
}

fn span_contains(s: &ExtractedSymbol, line: usize, col: usize) -> bool {
    let after_start = line > s.start_line || (line == s.start_line && col >= s.start_col);
    let before_end = line < s.end_line || (line == s.end_line && col <= s.end_col);
    after_start && before_end
}

fn span_size(s: &ExtractedSymbol) -> (usize, usize) {
    (s.end_line - s.start_line, s.end_col.abs_diff(s.start_col))
}

fn new_symbol(
    kind: &str,
    name: &str,
    qualified_name: String,
    container: Option<&str>,
    span: TsNode,
    content: &str,
    docstring: Option<String>,
) -> ExtractedSymbol {
    ExtractedSymbol {
        kind: kind.to_string(),
        name: name.to_string(),
        qualified_name,
        start_line: span.start_position().row + 1,
        end_line: span.end_position().row + 1,
        start_col: span.start_position().column,
        end_col: span.end_position().column,
        docstring,
        signature: extract_first_line(span, content),
        body: node_text(span, content),
        is_exported: false,
        is_async: false,
        container: container.map(|c| c.to_string()),
        visibility: None,
        return_type: None,
        is_static: false,
        is_abstract: false,
    }
}

fn field_text(node: TsNode, field: &str, content: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| node_text(n, content))
}

fn return_type_text(node: TsNode, content: &str) -> Option<String> {
    field_text(node, "return_type", content)
        .map(|t| t.trim().trim_start_matches(':').trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Strip generic arguments and surrounding whitespace: `Foo<T>` -> `Foo`, `Vec::<u8>` -> `Vec`.
fn strip_generics(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for ch in text.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth -= 1,
            c if depth == 0 && !c.is_whitespace() => out.push(c),
            _ => {}
        }
    }
    out.trim_end_matches("::").to_string()
}

fn strip_quotes(text: &str) -> String {
    text.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

fn is_rust_async(node: TsNode, content: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "async" {
            return true;
        }
        if child.kind() == "function_modifiers" {
            let mut mod_cursor = child.walk();
            for m in child.children(&mut mod_cursor) {
                if m.kind() == "async" {
                    return true;
                }
            }
        }
    }
    let first_line = extract_first_line(node, content).unwrap_or_default();
    first_line.contains("async fn")
}

fn extract_rust(content: &str) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = tree_sitter_rust::LANGUAGE.into();
    parser.set_language(&lang).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    let mut out = RustOut::default();
    walk_rust_node(root, content, &mut out, None, false);

    let mut calls = Vec::new();
    collect_rust_calls(root, content, &mut calls);
    collect_rust_instantiations(root, content, &mut out.refs);

    Some(ExtractionResult {
        language: "rust".to_string(),
        symbols: out.symbols,
        calls,
        imports: out.imports,
        trait_impls: out.trait_impls,
        refs: out.refs,
        parse_status: parse_status_of(root),
    })
}

fn parse_status_of(root: TsNode) -> String {
    if root.has_error() {
        "partial".to_string()
    } else {
        "ok".to_string()
    }
}

#[derive(Default)]
struct RustOut {
    /// Enclosing inline `mod name { .. }` blocks, outermost first.
    mod_path: Vec<String>,
    symbols: Vec<ExtractedSymbol>,
    imports: Vec<ExtractedImport>,
    trait_impls: Vec<ExtractedTraitImpl>,
    refs: Vec<ExtractedRef>,
}

/// Reference from a declared symbol (`from` = its qualified name) to a named target.
fn decl_ref(kind: &str, from: &ExtractedSymbol, target: &str, at: TsNode) -> ExtractedRef {
    ExtractedRef {
        kind: kind.to_string(),
        from: from.qualified_name.clone(),
        from_kind: from.kind.clone(),
        target_name: target.to_string(),
        qualifier: None,
        line: at.start_position().row + 1,
        col: at.start_position().column,
    }
}

/// Reference made from inside a body; the source is the enclosing callable at that position.
fn scoped_ref(kind: &str, target: &str, qualifier: Option<String>, at: TsNode) -> ExtractedRef {
    ExtractedRef {
        kind: kind.to_string(),
        from: String::new(),
        from_kind: String::new(),
        target_name: target.to_string(),
        qualifier,
        line: at.start_position().row + 1,
        col: at.start_position().column,
    }
}

const PRIMITIVE_TYPES: [&str; 40] = [
    "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    "f32", "f64", "bool", "char", "str", "String", "Self", "self", "string", "number",
    "boolean", "any", "unknown", "void", "never", "object", "undefined", "null", "int",
    "float", "bytes", "None", "dict", "list", "tuple", "set", "Any", "symbol",
];

const WRAPPER_TYPES: [&str; 24] = [
    "Option", "Vec", "Box", "Rc", "Arc", "Result", "RefCell", "Cell", "Mutex", "RwLock",
    "HashMap", "HashSet", "BTreeMap", "BTreeSet", "VecDeque", "Promise", "Array",
    "ReadonlyArray", "Partial", "Readonly", "Optional", "List", "Sequence", "Iterable",
];

/// The project type a type annotation most directly names: `&mut Foo` -> `Foo`,
/// `Option<Vec<Foo>>` -> `Foo`, `crate::m::Foo<T>` -> `Foo`, `Foo[]` -> `Foo`. Primitives and
/// unparseable annotations yield `None`.
pub(crate) fn primary_type_name(text: &str) -> Option<String> {
    let mut t = text.trim().trim_start_matches(':').trim().trim_start_matches("->").trim();
    loop {
        let before = t;
        t = t.trim_start_matches('&').trim_start();
        for prefix in ["mut ", "dyn ", "impl ", "const ", "readonly ", "typeof "] {
            if let Some(rest) = t.strip_prefix(prefix) {
                t = rest.trim_start();
            }
        }
        if t.starts_with('\'') {
            t = t.split_once(' ').map(|(_, r)| r).unwrap_or("").trim_start();
        }
        if t == before {
            break;
        }
    }
    let t = t.trim_end_matches("[]").trim();
    if t.is_empty() {
        return None;
    }
    let (head, generic) = match t.find(['<', '[']) {
        Some(pos) => (&t[..pos], Some(&t[pos + 1..])),
        None => (t, None),
    };
    let name = head
        .rsplit("::")
        .next()
        .unwrap_or(head)
        .rsplit('.')
        .next()
        .unwrap_or(head)
        .trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    if WRAPPER_TYPES.contains(&name) {
        let inner = generic?;
        // First generic argument only (`Result<Foo, E>` -> `Foo`).
        let mut depth = 0i32;
        let mut end = inner.len();
        for (i, ch) in inner.char_indices() {
            match ch {
                '<' | '[' | '(' => depth += 1,
                '>' | ']' | ')' if depth == 0 => {
                    end = i;
                    break;
                }
                '>' | ']' | ')' => depth -= 1,
                ',' if depth == 0 => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        return primary_type_name(&inner[..end]);
    }
    if PRIMITIVE_TYPES.contains(&name) || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(name.to_string())
}

/// `Foo { .. }` struct literals are instantiations of `Foo`.
fn collect_rust_instantiations(node: TsNode, content: &str, refs: &mut Vec<ExtractedRef>) {
    if node.kind() == "struct_expression" {
        if let Some(name) = node.child_by_field_name("name") {
            let text = strip_generics(&node_text(name, content));
            let mut segs: Vec<&str> = text.split("::").filter(|s| !s.is_empty()).collect();
            if let Some(last) = segs.pop() {
                if last != "Self" {
                    let qualifier = if segs.is_empty() {
                        None
                    } else {
                        Some(segs.join("::"))
                    };
                    refs.push(scoped_ref("instantiates", last, qualifier, node));
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_instantiations(child, content, refs);
    }
}

fn rust_visibility(node: TsNode, content: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return Some(
                node_text(child, content)
                    .split_whitespace()
                    .collect::<String>(),
            );
        }
    }
    None
}

fn rust_has_self_param(node: TsNode) -> bool {
    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for p in params.named_children(&mut cursor) {
            if p.kind() == "self_parameter" {
                return true;
            }
        }
    }
    false
}

fn walk_rust_children(
    node: TsNode,
    content: &str,
    out: &mut RustOut,
    container: Option<&str>,
    in_trait: bool,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_rust_node(child, content, out, container, in_trait);
    }
}

fn walk_rust_node(
    node: TsNode,
    content: &str,
    out: &mut RustOut,
    container: Option<&str>,
    in_trait: bool,
) {
    match node.kind() {
        "function_item" | "function_signature_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = match container {
                    Some(c) => format!("{}::{}", c, name),
                    None if !out.mod_path.is_empty() => {
                        format!("{}::{}", out.mod_path.join("::"), name)
                    }
                    None => name.clone(),
                };
                let kind = if container.is_some() {
                    "method"
                } else {
                    "function"
                };
                let mut sym = new_symbol(
                    kind,
                    &name,
                    qualified,
                    container,
                    node,
                    content,
                    rust_doc_comments(node, content),
                );
                let vis = rust_visibility(node, content);
                sym.is_exported = vis.is_some() || in_trait;
                sym.visibility = Some(vis.unwrap_or_else(|| {
                    if in_trait {
                        "pub".to_string()
                    } else {
                        "private".to_string()
                    }
                }));
                sym.is_async = is_rust_async(node, content);
                sym.return_type = return_type_text(node, content);
                sym.is_abstract = node.kind() == "function_signature_item";
                sym.is_static = container.is_some() && !rust_has_self_param(node);
                if let Some(rt) = node.child_by_field_name("return_type") {
                    if let Some(t) = primary_type_name(&node_text(rt, content)) {
                        out.refs.push(decl_ref("returns", &sym, &t, rt));
                    }
                }
                out.symbols.push(sym);
            }
            // Nested items inside the body are free-standing (no container).
            if let Some(body) = node.child_by_field_name("body") {
                walk_rust_children(body, content, out, None, false);
            }
            return;
        }
        "struct_item" | "enum_item" | "union_item" | "type_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let kind = match node.kind() {
                    "enum_item" => "enum",
                    "type_item" => "type_alias",
                    _ => "struct",
                };
                let mut sym = new_symbol(
                    kind,
                    &name,
                    name.clone(),
                    None,
                    node,
                    content,
                    rust_doc_comments(node, content),
                );
                let vis = rust_visibility(node, content);
                sym.is_exported = vis.is_some();
                sym.visibility = Some(vis.unwrap_or_else(|| "private".to_string()));
                let exported = sym.is_exported;
                out.symbols.push(sym);
                if let Some(body) = node.child_by_field_name("body") {
                    rust_type_members(body, content, out, &name, exported);
                }
            }
            return;
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let mut sym = new_symbol(
                    "trait",
                    &name,
                    name.clone(),
                    None,
                    node,
                    content,
                    rust_doc_comments(node, content),
                );
                let vis = rust_visibility(node, content);
                sym.is_exported = vis.is_some();
                sym.visibility = Some(vis.unwrap_or_else(|| "private".to_string()));
                sym.is_abstract = true;
                out.symbols.push(sym);

                if let Some(body_node) = node.child_by_field_name("body") {
                    walk_rust_children(body_node, content, out, Some(&name), true);
                }
            }
            return;
        }
        "impl_item" => {
            let trait_name = node
                .child_by_field_name("trait")
                .map(|n| strip_generics(&node_text(n, content)));
            let type_name = node
                .child_by_field_name("type")
                .map(|n| strip_generics(&node_text(n, content)));

            if let (Some(tr), Some(ty)) = (&trait_name, &type_name) {
                out.trait_impls.push(ExtractedTraitImpl {
                    trait_name: tr.clone(),
                    type_name: ty.clone(),
                    line: node.start_position().row + 1,
                });
            }

            // Use the bare type name as container: `impl fmt::Display for crate::x::Foo` -> `Foo`.
            let container = type_name
                .as_deref()
                .map(|t| t.rsplit("::").next().unwrap_or(t).to_string())
                .unwrap_or_else(|| "impl".to_string());

            if let Some(body_node) = node.child_by_field_name("body") {
                walk_rust_children(body_node, content, out, Some(&container), false);
            }
            return;
        }
        "const_item" | "static_item" => {
            // `const` items are constants, `static` items variables;
            // associated consts of an impl / trait belong to that type.
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = match container {
                    Some(c) => format!("{}::{}", c, name),
                    None if !out.mod_path.is_empty() => {
                        format!("{}::{}", out.mod_path.join("::"), name)
                    }
                    None => name.clone(),
                };
                let kind = if node.kind() == "const_item" { "constant" } else { "variable" };
                let mut sym = new_symbol(
                    kind,
                    &name,
                    qualified,
                    container,
                    node,
                    content,
                    rust_doc_comments(node, content),
                );
                let vis = rust_visibility(node, content);
                sym.is_exported = vis.is_some() || in_trait;
                let default_vis = if in_trait { "pub" } else { "private" };
                sym.visibility = Some(vis.unwrap_or_else(|| default_vis.to_string()));
                sym.is_static = node.kind() == "static_item";
                if let Some(ty) = node.child_by_field_name("type") {
                    if let Some(t) = primary_type_name(&node_text(ty, content)) {
                        out.refs.push(decl_ref("type_of", &sym, &t, ty));
                    }
                }
                out.symbols.push(sym);
            }
            return;
        }
        "mod_item" if node.child_by_field_name("body").is_none() => {
            // `mod util;` declares a module whose items live in `util.rs` / `util/mod.rs`; the
            // namespace node is linked to that file at build time (`rust_mod` link).
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = if out.mod_path.is_empty() {
                    name.clone()
                } else {
                    format!("{}::{}", out.mod_path.join("::"), name)
                };
                let mut sym = new_symbol(
                    "namespace",
                    &name,
                    qualified.clone(),
                    None,
                    node,
                    content,
                    rust_doc_comments(node, content),
                );
                let vis = rust_visibility(node, content);
                sym.is_exported = vis.is_some();
                sym.visibility = Some(vis.unwrap_or_else(|| "private".to_string()));
                let explicit_path = rust_path_attribute(node, content);
                out.refs.push(ExtractedRef {
                    kind: LINK_RUST_MOD.to_string(),
                    from: qualified,
                    from_kind: "namespace".to_string(),
                    target_name: name,
                    qualifier: Some(match explicit_path {
                        Some(p) => format!("path:{}", p),
                        None => out.mod_path.join("/"),
                    }),
                    line: node.start_position().row + 1,
                    col: node.start_position().column,
                });
                out.symbols.push(sym);
            }
            return;
        }
        "mod_item" => {
            if let (Some(name_node), Some(body)) =
                (node.child_by_field_name("name"), node.child_by_field_name("body"))
            {
                let name = node_text(name_node, content);
                let qualified = if out.mod_path.is_empty() {
                    name.clone()
                } else {
                    format!("{}::{}", out.mod_path.join("::"), name)
                };
                let mut sym = new_symbol(
                    "namespace",
                    &name,
                    qualified,
                    None,
                    node,
                    content,
                    rust_doc_comments(node, content),
                );
                let vis = rust_visibility(node, content);
                sym.is_exported = vis.is_some();
                sym.visibility = Some(vis.unwrap_or_else(|| "private".to_string()));
                out.symbols.push(sym);
                out.mod_path.push(name);
                walk_rust_children(body, content, out, None, false);
                out.mod_path.pop();
                return;
            }
        }
        "use_declaration" => {
            let line = node.start_position().row + 1;
            if let Some(arg) = node.child_by_field_name("argument") {
                let is_pub = rust_visibility(node, content).is_some();
                rust_use_tree(arg, content, &[], line, is_pub, &mut out.imports);
            }
            return;
        }
        _ => {}
    }

    walk_rust_children(node, content, out, container, in_trait);
}

/// `#[path = "file.rs"]` on a `mod name;` declaration.
fn rust_path_attribute(node: TsNode, content: &str) -> Option<String> {
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() != "attribute_item" {
            break;
        }
        let text = node_text(p, content);
        let inner = text.trim().trim_start_matches("#[").trim_end_matches(']').trim();
        if let Some(rest) = inner.strip_prefix("path") {
            let rest = rest.trim_start();
            if let Some(v) = rest.strip_prefix('=') {
                let v = strip_quotes(v);
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        prev = p.prev_named_sibling();
    }
    None
}

/// Enum variants (`enum_member`) and named struct fields (`property`, with a `type_of`
/// reference to the field's type).
fn rust_type_members(body: TsNode, content: &str, out: &mut RustOut, owner: &str, exported: bool) {
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        match member.kind() {
            "enum_variant" => {
                if let Some(n) = member.child_by_field_name("name") {
                    let name = node_text(n, content);
                    let mut sym = new_symbol(
                        "enum_member",
                        &name,
                        format!("{}::{}", owner, name),
                        Some(owner),
                        member,
                        content,
                        rust_doc_comments(member, content),
                    );
                    sym.is_exported = exported;
                    sym.visibility = Some(if exported { "pub" } else { "private" }.to_string());
                    out.symbols.push(sym);
                }
            }
            "field_declaration" => {
                if let Some(n) = member.child_by_field_name("name") {
                    let name = node_text(n, content);
                    let mut sym = new_symbol(
                        "property",
                        &name,
                        format!("{}::{}", owner, name),
                        Some(owner),
                        member,
                        content,
                        rust_doc_comments(member, content),
                    );
                    let vis = rust_visibility(member, content);
                    sym.is_exported = vis.is_some();
                    sym.visibility = Some(vis.unwrap_or_else(|| "private".to_string()));
                    if let Some(ty) = member.child_by_field_name("type") {
                        if let Some(t) = primary_type_name(&node_text(ty, content)) {
                            out.refs.push(decl_ref("type_of", &sym, &t, ty));
                        }
                    }
                    out.symbols.push(sym);
                }
            }
            _ => {}
        }
    }
}

fn rust_path_segments(text: &str) -> Vec<String> {
    text.split("::")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn rust_emit_use(
    segs: Vec<String>,
    alias: Option<String>,
    line: usize,
    is_pub: bool,
    out: &mut Vec<ExtractedImport>,
) {
    let mut segs = segs;
    if segs.last().map(|s| s == "self").unwrap_or(false) {
        segs.pop();
    }
    if segs.is_empty() {
        return;
    }
    let last = segs.last().cloned().unwrap_or_default();
    let (module, is_module) = if segs.len() == 1 {
        (last.clone(), true)
    } else {
        (segs[..segs.len() - 1].join("::"), false)
    };
    out.push(ExtractedImport {
        imported_name: last.clone(),
        source_module: module,
        local_name: alias.unwrap_or(last),
        is_module,
        is_type_only: false,
        is_reexport: is_pub,
        line,
    });
}

fn rust_use_tree(
    node: TsNode,
    content: &str,
    prefix: &[String],
    line: usize,
    is_pub: bool,
    out: &mut Vec<ExtractedImport>,
) {
    match node.kind() {
        "scoped_use_list" => {
            let mut p = prefix.to_vec();
            if let Some(path) = node.child_by_field_name("path") {
                p.extend(rust_path_segments(&node_text(path, content)));
            }
            if let Some(list) = node.child_by_field_name("list") {
                rust_use_tree(list, content, &p, line, is_pub, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                rust_use_tree(child, content, prefix, line, is_pub, out);
            }
        }
        "use_as_clause" => {
            let mut segs = prefix.to_vec();
            if let Some(path) = node.child_by_field_name("path") {
                segs.extend(rust_path_segments(&node_text(path, content)));
            }
            let alias = field_text(node, "alias", content);
            rust_emit_use(segs, alias, line, is_pub, out);
        }
        "use_wildcard" => {
            let text = node_text(node, content);
            let mut segs = prefix.to_vec();
            segs.extend(rust_path_segments(text.trim_end_matches('*')));
            if segs.is_empty() {
                return;
            }
            out.push(ExtractedImport {
                imported_name: "*".to_string(),
                source_module: segs.join("::"),
                local_name: String::new(),
                is_module: true,
                is_type_only: false,
                is_reexport: is_pub,
                line,
            });
        }
        "identifier" | "scoped_identifier" | "crate" | "self" | "super" | "metavariable" => {
            let mut segs = prefix.to_vec();
            segs.extend(rust_path_segments(&node_text(node, content)));
            rust_emit_use(segs, None, line, is_pub, out);
        }
        _ => {}
    }
}

fn collect_rust_calls(node: TsNode, content: &str, calls: &mut Vec<ExtractedCall>) {
    if node.kind() == "macro_invocation" {
        // Macro arguments are an unparsed token tree (`println!("{}", f(x))`), so calls inside
        // them never appear as `call_expression` nodes. Scan the tokens for call shapes instead.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "token_tree" {
                collect_rust_token_tree_calls(child, content, calls);
            }
        }
        return;
    }
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            if let Some(call) = rust_call_target(func, content, node) {
                calls.push(call);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_calls(child, content, calls);
    }
}

/// Path-segment tokens that may appear in a call path inside a macro token tree.
fn is_rust_path_token(kind: &str) -> bool {
    matches!(kind, "identifier" | "self" | "super" | "crate" | "Self")
}

/// Recognise call shapes in a macro token tree: `f(..)`, `a::b::f(..)` and `recv.m(..)`.
/// Nested macro names (`format!(..)`) and `fn name(..)` definitions are not calls, but their
/// own token trees are still scanned.
fn collect_rust_token_tree_calls(tree: TsNode, content: &str, calls: &mut Vec<ExtractedCall>) {
    let mut cursor = tree.walk();
    let toks: Vec<TsNode> = tree.children(&mut cursor).collect();
    for (i, tok) in toks.iter().enumerate() {
        if tok.kind() != "token_tree" {
            continue;
        }
        collect_rust_token_tree_calls(*tok, content, calls);
        if !node_text(*tok, content).starts_with('(') || i == 0 {
            continue;
        }
        let name_tok = toks[i - 1];
        if name_tok.kind() != "identifier" {
            continue;
        }
        let before = |j: usize| -> Option<TsNode> { j.checked_sub(1).map(|k| toks[k]) };
        if before(i - 1).is_some_and(|p| p.kind() == "fn") {
            continue;
        }
        let target = node_text(name_tok, content);
        let mut start = name_tok;
        let mut receiver = None;
        let mut qualifier = None;
        let mut is_method = false;
        match before(i - 1).map(|p| p.kind()) {
            Some(".") => {
                is_method = true;
                receiver = before(i - 2)
                    .filter(|r| is_rust_path_token(r.kind()))
                    .map(|r| node_text(r, content));
            }
            Some("::") => {
                // Walk back over `seg ::` pairs to build the qualifier path.
                let mut segs: Vec<String> = Vec::new();
                let mut j = i - 1;
                while j >= 2 && toks[j - 1].kind() == "::" && is_rust_path_token(toks[j - 2].kind()) {
                    segs.push(node_text(toks[j - 2], content));
                    start = toks[j - 2];
                    j -= 2;
                }
                if !segs.is_empty() {
                    segs.reverse();
                    qualifier = Some(segs.join("::"));
                }
            }
            _ => {}
        }
        calls.push(ExtractedCall {
            caller_name: String::new(),
            target_name: target,
            receiver,
            qualifier,
            receiver_type: None,
            is_method,
            line: start.start_position().row + 1,
            col: start.start_position().column,
        });
    }
}

fn rust_call_target(func: TsNode, content: &str, call_node: TsNode) -> Option<ExtractedCall> {
    let mut func = func;
    loop {
        if func.kind() == "generic_function" {
            func = func.child_by_field_name("function")?;
            continue;
        }
        break;
    }
    let (target, receiver, qualifier, is_method) = match func.kind() {
        "identifier" => (node_text(func, content), None, None, false),
        "scoped_identifier" => {
            let name = field_text(func, "name", content)?;
            let path = func
                .child_by_field_name("path")
                .map(|p| strip_generics(&node_text(p, content)));
            (name, None, path, false)
        }
        "field_expression" => {
            let field = func.child_by_field_name("field")?;
            if field.kind() != "field_identifier" {
                return None;
            }
            let recv = field_text(func, "value", content).map(|r| r.trim().to_string());
            (node_text(field, content), recv, None, true)
        }
        _ => return None,
    };
    if target.is_empty() {
        return None;
    }
    Some(ExtractedCall {
        caller_name: String::new(),
        target_name: target,
        receiver,
        qualifier,
        receiver_type: None,
        is_method,
        line: call_node.start_position().row + 1,
        col: call_node.start_position().column,
    })
}

/// Rust doc comments (`///`, `/** */`) attached to an item. Attributes such as `#[derive(..)]`
/// sitting between the comment and the item are skipped.
fn rust_doc_comments(node: TsNode, content: &str) -> Option<String> {
    let mut docs: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(sib) = cur {
        match sib.kind() {
            "attribute_item" => {}
            "line_comment" | "block_comment" => {
                let text = node_text(sib, content);
                let t = text.trim();
                if (t.starts_with("///") && !t.starts_with("////")) || t.starts_with("/**") {
                    docs.push(t.to_string());
                } else {
                    break;
                }
            }
            _ => break,
        }
        cur = sib.prev_sibling();
    }
    if docs.is_empty() {
        None
    } else {
        docs.reverse();
        Some(docs.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Reference helpers shared by the TS/JS (`typescript.rs`), C# and Python extractors
// ---------------------------------------------------------------------------

/// Reference from a declared symbol to a (possibly qualified) name.
fn named_ref(
    kind: &str,
    from: &ExtractedSymbol,
    target: &str,
    qualifier: Option<String>,
    at: TsNode,
) -> ExtractedRef {
    let mut r = decl_ref(kind, from, target, at);
    r.qualifier = qualifier;
    r
}

/// `models.Base<T>` -> (`Base`, Some(`models`)). Call expressions and other computed shapes
/// yield `None`.
fn split_type_ref(text: &str) -> Option<(String, Option<String>)> {
    let text = strip_generics(text);
    if text.is_empty() || text.contains(['(', '[', '{', '|', '&', '"', '\'']) {
        return None;
    }
    let (q, name) = match text.rsplit_once('.') {
        Some((q, n)) => (Some(q.to_string()), n.to_string()),
        None => match text.rsplit_once("::") {
            Some((q, n)) => (Some(q.to_string()), n.to_string()),
            None => (None, text.clone()),
        },
    };
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
        return None;
    }
    Some((name, q))
}

fn push_returns(node: TsNode, content: &str, sym: &ExtractedSymbol, refs: &mut Vec<ExtractedRef>) {
    if let Some(rt) = node.child_by_field_name("return_type") {
        if let Some(t) = primary_type_name(&node_text(rt, content)) {
            refs.push(decl_ref("returns", sym, &t, rt));
        }
    }
}

/// Bare identifiers passed as call arguments (`app.get("/", handler)`): function references.
fn collect_identifier_args(args: TsNode, content: &str, refs: &mut Vec<ExtractedRef>) {
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        let ident = match arg.kind() {
            "identifier" => Some(arg),
            "keyword_argument" => arg.child_by_field_name("value").filter(|v| v.kind() == "identifier"),
            _ => None,
        };
        if let Some(id) = ident {
            let name = node_text(id, content);
            if !matches!(name.as_str(), "undefined" | "None" | "self" | "this" | "cls") {
                refs.push(scoped_ref("references", &name, None, id));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PyOut {
    /// Qualified names of module / class variables already declared (first binding wins).
    declared: std::collections::HashSet<String>,
    symbols: Vec<ExtractedSymbol>,
    imports: Vec<ExtractedImport>,
    refs: Vec<ExtractedRef>,
}

/// `@name`, `@mod.name` and `@name(...)` decorators on a definition.
fn py_decorator_refs(node: TsNode, content: &str, sym: &ExtractedSymbol, refs: &mut Vec<ExtractedRef>) {
    let Some(parent) = node.parent().filter(|p| p.kind() == "decorated_definition") else {
        return;
    };
    let mut cursor = parent.walk();
    for d in parent.named_children(&mut cursor) {
        if d.kind() != "decorator" {
            continue;
        }
        let Some(mut expr) = d.named_child(0) else { continue };
        if expr.kind() == "call" {
            match expr.child_by_field_name("function") {
                Some(f) => expr = f,
                None => continue,
            }
        }
        if let Some((name, q)) = split_type_ref(&node_text(expr, content)) {
            refs.push(named_ref("decorates", sym, &name, q, d));
        }
    }
}

fn extract_python(content: &str) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = tree_sitter_python::LANGUAGE.into();
    parser.set_language(&lang).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    let mut out = PyOut::default();
    walk_py_node(root, content, &mut out, None);

    let mut calls = Vec::new();
    collect_py_calls(root, content, &mut calls, &mut out.refs);

    Some(ExtractionResult {
        language: "python".to_string(),
        symbols: out.symbols,
        calls,
        imports: out.imports,
        trait_impls: Vec::new(),
        refs: out.refs,
        parse_status: parse_status_of(root),
    })
}

fn py_decorators(node: TsNode, content: &str) -> Vec<String> {
    let mut decorators = Vec::new();
    if let Some(parent) = node.parent() {
        if parent.kind() == "decorated_definition" {
            let mut cursor = parent.walk();
            for child in parent.named_children(&mut cursor) {
                if child.kind() == "decorator" {
                    decorators.push(node_text(child, content).trim().to_string());
                }
            }
        }
    }
    decorators
}

fn py_visibility(name: &str) -> String {
    if name.starts_with('_') && !(name.starts_with("__") && name.ends_with("__")) {
        "private".to_string()
    } else {
        "public".to_string()
    }
}

fn walk_py_children(node: TsNode, content: &str, out: &mut PyOut, container: Option<&str>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_py_node(child, content, out, container);
    }
}

fn walk_py_node(node: TsNode, content: &str, out: &mut PyOut, container: Option<&str>) {
    match node.kind() {
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = match container {
                    Some(c) => format!("{}.{}", c, name),
                    None => name.clone(),
                };
                let kind = if container.is_some() {
                    "method"
                } else {
                    "function"
                };
                let mut sym = new_symbol(
                    kind,
                    &name,
                    qualified,
                    container,
                    node,
                    content,
                    extract_python_docstring(node, content),
                );
                let decorators = py_decorators(node, content);
                let vis = py_visibility(&name);
                sym.is_exported = vis == "public";
                sym.visibility = Some(vis);
                sym.is_async = node_has_child_kind(node, "async");
                sym.return_type = return_type_text(node, content);
                sym.is_static = decorators.iter().any(|d| d.contains("staticmethod"));
                sym.is_abstract = decorators.iter().any(|d| d.contains("abstractmethod"));
                push_returns(node, content, &sym, &mut out.refs);
                py_decorator_refs(node, content, &sym, &mut out.refs);
                out.symbols.push(sym);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_py_children(body, content, out, None);
            }
            return;
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let mut sym = new_symbol(
                    "class",
                    &name,
                    name.clone(),
                    None,
                    node,
                    content,
                    extract_python_docstring(node, content),
                );
                let vis = py_visibility(&name);
                sym.is_exported = vis == "public";
                sym.visibility = Some(vis);
                sym.is_abstract = field_text(node, "superclasses", content)
                    .map(|s| s.contains("ABC"))
                    .unwrap_or(false);
                if let Some(supers) = node.child_by_field_name("superclasses") {
                    let mut sc = supers.walk();
                    for base in supers.named_children(&mut sc) {
                        if !matches!(base.kind(), "identifier" | "attribute") {
                            continue;
                        }
                        if let Some((n, q)) = split_type_ref(&node_text(base, content)) {
                            if n != "object" {
                                out.refs.push(named_ref("extends", &sym, &n, q, base));
                            }
                        }
                    }
                }
                py_decorator_refs(node, content, &sym, &mut out.refs);
                out.symbols.push(sym);

                if let Some(body_node) = node.child_by_field_name("body") {
                    walk_py_children(body_node, content, out, Some(&name));
                }
            }
            return;
        }
        "assignment" => {
            py_variable(node, content, out);
            // Lambdas / comprehensions in the value hold no declarations of interest.
            return;
        }
        "import_statement" => {
            let line = node.start_position().row + 1;
            let mut cursor = node.walk();
            for child in node.children_by_field_name("name", &mut cursor) {
                let (module, alias) = match child.kind() {
                    "dotted_name" => (node_text(child, content), None),
                    "aliased_import" => (
                        field_text(child, "name", content).unwrap_or_default(),
                        field_text(child, "alias", content),
                    ),
                    _ => continue,
                };
                if module.is_empty() {
                    continue;
                }
                // `import a.b` binds `a`; `import a.b as c` binds `c`.
                let local =
                    alias.unwrap_or_else(|| module.split('.').next().unwrap_or("").to_string());
                out.imports.push(ExtractedImport {
                    imported_name: "*".to_string(),
                    source_module: module,
                    local_name: local,
                    is_module: true,
                    is_type_only: false,
                    is_reexport: false,
                    line,
                });
            }
            return;
        }
        "import_from_statement" => {
            let line = node.start_position().row + 1;
            let module = field_text(node, "module_name", content)
                .map(|m| m.split_whitespace().collect::<String>())
                .unwrap_or_default();
            if module.is_empty() {
                return;
            }
            let mut any = false;
            let mut cursor = node.walk();
            for child in node.children_by_field_name("name", &mut cursor) {
                let (imported, alias) = match child.kind() {
                    "dotted_name" => (node_text(child, content), None),
                    "aliased_import" => (
                        field_text(child, "name", content).unwrap_or_default(),
                        field_text(child, "alias", content),
                    ),
                    _ => continue,
                };
                if imported.is_empty() {
                    continue;
                }
                any = true;
                let local = alias.unwrap_or_else(|| imported.clone());
                out.imports.push(ExtractedImport {
                    imported_name: imported,
                    source_module: module.clone(),
                    local_name: local,
                    is_module: false,
                    is_type_only: false,
                    is_reexport: false,
                    line,
                });
            }
            if !any && node_has_child_kind(node, "wildcard_import") {
                out.imports.push(ExtractedImport {
                    imported_name: "*".to_string(),
                    source_module: module,
                    local_name: String::new(),
                    is_module: true,
                    is_type_only: false,
                    is_reexport: false,
                    line,
                });
            }
            return;
        }
        _ => {}
    }

    walk_py_children(node, content, out, container);
}

/// Where a Python assignment binds its name: the module, a class body (`Some(class)`), or a
/// function scope (`None`, not a declaration of the graph).
enum PyScope {
    Module,
    Class(String),
}

fn py_assignment_scope(node: TsNode, content: &str) -> Option<PyScope> {
    let statement = node.parent().filter(|p| p.kind() == "expression_statement")?;
    let mut cur = statement.parent();
    while let Some(p) = cur {
        match p.kind() {
            "module" => return Some(PyScope::Module),
            "function_definition" | "lambda" => return None,
            "class_definition" => {
                // Only the class body itself (a nested def would have returned above).
                return field_text(p, "name", content).map(PyScope::Class);
            }
            _ => cur = p.parent(),
        }
    }
    None
}

/// Module- and class-level variables: `NAME = ..` at module scope is a `constant` when written
/// in UPPER_CASE (the Python convention), any other binding a `variable`. Only the first
/// binding of a name is a declaration; tuple targets are skipped.
fn py_variable(node: TsNode, content: &str, out: &mut PyOut) {
    let Some(scope) = py_assignment_scope(node, content) else { return };
    let Some(left) = node.child_by_field_name("left") else { return };
    if left.kind() != "identifier" {
        return;
    }
    let name = node_text(left, content);
    if name.is_empty() {
        return;
    }
    let (qualified, container, is_const) = match &scope {
        PyScope::Module => {
            let is_const = name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && name.chars().any(|c| c.is_ascii_uppercase());
            (name.clone(), None, is_const)
        }
        PyScope::Class(c) => (format!("{}.{}", c, name), Some(c.as_str()), false),
    };
    if !out.declared.insert(qualified.clone()) {
        return;
    }
    let kind = if is_const { "constant" } else { "variable" };
    let mut sym = new_symbol(kind, &name, qualified, container, node, content, None);
    let vis = py_visibility(&name);
    sym.is_exported = vis == "public" && container.is_none();
    sym.visibility = Some(vis);
    sym.is_static = container.is_some();
    if let Some(right) = node.child_by_field_name("right") {
        let text = node_text(right, content);
        let cut: String = text.chars().take(200).collect();
        sym.signature = Some(format!("{} = {}", name, cut.lines().next().unwrap_or("")));
    }
    if let Some(ty) = node.child_by_field_name("type") {
        if let Some(t) = primary_type_name(&node_text(ty, content)) {
            out.refs.push(decl_ref("type_of", &sym, &t, ty));
        }
    }
    out.symbols.push(sym);
}

fn collect_py_calls(
    node: TsNode,
    content: &str,
    calls: &mut Vec<ExtractedCall>,
    refs: &mut Vec<ExtractedRef>,
) {
    if node.kind() == "call" {
        if let Some(args) = node.child_by_field_name("arguments") {
            collect_identifier_args(args, content, refs);
        }
        if let Some(func) = node.child_by_field_name("function") {
            let parsed = match func.kind() {
                "identifier" => Some((node_text(func, content), None, false)),
                "attribute" => {
                    let attr = field_text(func, "attribute", content);
                    let obj = field_text(func, "object", content).map(|o| o.trim().to_string());
                    attr.map(|a| (a, obj, true))
                }
                _ => None,
            };
            if let Some((target, receiver, is_method)) = parsed {
                if !target.is_empty() {
                    calls.push(ExtractedCall {
                        caller_name: String::new(),
                        target_name: target,
                        receiver,
                        qualifier: None,
                        receiver_type: None,
                        is_method,
                        line: node.start_position().row + 1,
                        col: node.start_position().column,
                    });
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_py_calls(child, content, calls, refs);
    }
}

// ---------------------------------------------------------------------------
// SQL / JSON / YAML / Markdown ADRs (line-based)
// ---------------------------------------------------------------------------

fn extract_sql(content: &str) -> Option<ExtractionResult> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let mut current_table: Option<String> = None;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with("CREATE TABLE") {
            let rest = trimmed["CREATE TABLE".len()..].trim();
            let without_if = if rest.to_uppercase().starts_with("IF NOT EXISTS") {
                rest["IF NOT EXISTS".len()..].trim()
            } else {
                rest
            };
            let table_name = without_if
                .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .trim_matches('`')
                .split('.')
                .next_back()
                .unwrap_or("")
                .to_string();

            if !table_name.is_empty() {
                current_table = Some(table_name.clone());
                symbols.push(ExtractedSymbol {
                    kind: "table".to_string(),
                    name: table_name.clone(),
                    qualified_name: table_name,
                    start_line: idx + 1,
                    end_line: idx + 1,
                    start_col: 0,
                    end_col: line.len(),
                    docstring: None,
                    signature: Some(trimmed.to_string()),
                    body: trimmed.to_string(),
                    is_exported: true,
                    is_async: false,
                    container: None,
                    visibility: None,
                    return_type: None,
                    is_static: false,
                    is_abstract: false,
                });
            }
        } else if upper.starts_with("CREATE TRIGGER")
            || upper.starts_with("CREATE OR REPLACE TRIGGER")
        {
            let rest = trimmed
                .split_whitespace()
                .skip(2)
                .collect::<Vec<_>>()
                .join(" ");
            let trig_name = rest
                .split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .to_string();
            if !trig_name.is_empty() {
                symbols.push(ExtractedSymbol {
                    kind: "trigger".to_string(),
                    name: trig_name.clone(),
                    qualified_name: trig_name,
                    start_line: idx + 1,
                    end_line: idx + 1,
                    start_col: 0,
                    end_col: line.len(),
                    docstring: None,
                    signature: Some(trimmed.to_string()),
                    body: trimmed.to_string(),
                    is_exported: true,
                    is_async: false,
                    container: None,
                    visibility: None,
                    return_type: None,
                    is_static: false,
                    is_abstract: false,
                });
            }
        } else if upper.starts_with("CREATE INDEX") || upper.starts_with("CREATE UNIQUE INDEX") {
            let skip_count = if upper.starts_with("CREATE UNIQUE INDEX") {
                3
            } else {
                2
            };
            let rest = trimmed
                .split_whitespace()
                .skip(skip_count)
                .collect::<Vec<_>>()
                .join(" ");
            let rest = if rest.to_uppercase().starts_with("IF NOT EXISTS") {
                rest["IF NOT EXISTS".len()..].trim()
            } else {
                rest.as_str()
            };
            let idx_name = rest
                .split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .to_string();
            if !idx_name.is_empty() {
                symbols.push(ExtractedSymbol {
                    kind: "index".to_string(),
                    name: idx_name.clone(),
                    qualified_name: idx_name,
                    start_line: idx + 1,
                    end_line: idx + 1,
                    start_col: 0,
                    end_col: line.len(),
                    docstring: None,
                    signature: Some(trimmed.to_string()),
                    body: trimmed.to_string(),
                    is_exported: true,
                    is_async: false,
                    container: None,
                    visibility: None,
                    return_type: None,
                    is_static: false,
                    is_abstract: false,
                });
            }
        } else if upper.contains("CONSTRAINT") && upper.contains("CHECK") {
            if let Some(pos) = upper.find("CONSTRAINT") {
                let rest = trimmed[pos + "CONSTRAINT".len()..].trim();
                let constraint_name = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"');
                if !constraint_name.is_empty() {
                    let qualified = match &current_table {
                        Some(t) => format!("{}::{}", t, constraint_name),
                        None => constraint_name.to_string(),
                    };
                    symbols.push(ExtractedSymbol {
                        kind: "check_constraint".to_string(),
                        name: constraint_name.to_string(),
                        qualified_name: qualified,
                        start_line: idx + 1,
                        end_line: idx + 1,
                        start_col: 0,
                        end_col: line.len(),
                        docstring: None,
                        signature: Some(trimmed.to_string()),
                        body: trimmed.to_string(),
                        is_exported: true,
                        is_async: false,
                        container: None,
                        visibility: None,
                        return_type: None,
                        is_static: false,
                        is_abstract: false,
                    });
                }
            }
        }
    }

    if symbols.is_empty() {
        None
    } else {
        Some(ExtractionResult {
            language: "sql".to_string(),
            symbols,
            calls: Vec::new(),
            imports: Vec::new(),
            trait_impls: Vec::new(),
            refs: Vec::new(),
            parse_status: default_parse_status(),
        })
    }
}

fn extract_json_or_yaml(path: &str, content: &str) -> Option<ExtractionResult> {
    let val: serde_json::Value = if path.ends_with(".yaml") || path.ends_with(".yml") {
        serde_yaml::from_str(content).ok()?
    } else {
        serde_json::from_str(content).ok()?
    };

    let mut symbols = Vec::new();
    let is_openapi = val.get("openapi").is_some() || val.get("swagger").is_some();

    if is_openapi {
        if let Some(paths) = val.get("paths").and_then(|p| p.as_object()) {
            for (path_str, methods) in paths {
                if let Some(methods_obj) = methods.as_object() {
                    for (method, _def) in methods_obj {
                        let method_upper = method.to_uppercase();
                        if ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"]
                            .contains(&method_upper.as_str())
                        {
                            let ep_name = format!("{} {}", method_upper, path_str);
                            symbols.push(ExtractedSymbol {
                                kind: "endpoint".to_string(),
                                name: ep_name.clone(),
                                qualified_name: ep_name.clone(),
                                start_line: 1,
                                end_line: 1,
                                start_col: 0,
                                end_col: 0,
                                docstring: None,
                                signature: Some(ep_name),
                                body: String::new(),
                                is_exported: true,
                                is_async: false,
                                container: None,
                                visibility: None,
                                return_type: None,
                                is_static: false,
                                is_abstract: false,
                            });
                        }
                    }
                }
            }
        }

        if let Some(schemas) = val
            .pointer("/components/schemas")
            .and_then(|s| s.as_object())
        {
            for (schema_name, _schema_def) in schemas {
                symbols.push(ExtractedSymbol {
                    kind: "schema".to_string(),
                    name: schema_name.clone(),
                    qualified_name: format!("components.schemas.{}", schema_name),
                    start_line: 1,
                    end_line: 1,
                    start_col: 0,
                    end_col: 0,
                    docstring: None,
                    signature: Some(schema_name.clone()),
                    body: String::new(),
                    is_exported: true,
                    is_async: false,
                    container: None,
                    visibility: None,
                    return_type: None,
                    is_static: false,
                    is_abstract: false,
                });
            }
        }
    } else if val.get("$schema").is_some() || val.get("properties").is_some() {
        let name = val
            .get("title")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("schema")
                    .to_string()
            });
        symbols.push(ExtractedSymbol {
            kind: "schema".to_string(),
            name: name.clone(),
            qualified_name: name.clone(),
            start_line: 1,
            end_line: 1,
            start_col: 0,
            end_col: 0,
            docstring: None,
            signature: Some(name),
            body: String::new(),
            is_exported: true,
            is_async: false,
            container: None,
            visibility: None,
            return_type: None,
            is_static: false,
            is_abstract: false,
        });
    }

    if symbols.is_empty() {
        None
    } else {
        Some(ExtractionResult {
            language: if path.ends_with(".yaml") || path.ends_with(".yml") {
                "yaml".to_string()
            } else {
                "json".to_string()
            },
            symbols,
            calls: Vec::new(),
            imports: Vec::new(),
            trait_impls: Vec::new(),
            refs: Vec::new(),
            parse_status: default_parse_status(),
        })
    }
}

fn extract_markdown_adr(path: &str, content: &str) -> Option<ExtractionResult> {
    let mut title = None;
    let mut line_no = 1;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix("# ") {
            title = Some(stripped.trim().to_string());
            line_no = idx + 1;
            break;
        }
    }

    let adr_title = title.unwrap_or_else(|| {
        std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("ADR")
            .to_string()
    });

    let symbol = ExtractedSymbol {
        kind: "adr".to_string(),
        name: adr_title.clone(),
        qualified_name: adr_title.clone(),
        start_line: line_no,
        end_line: line_no,
        start_col: 0,
        end_col: 0,
        docstring: None,
        signature: Some(adr_title),
        body: content.to_string(),
        is_exported: true,
        is_async: false,
        container: None,
        visibility: None,
        return_type: None,
        is_static: false,
        is_abstract: false,
    };

    Some(ExtractionResult {
        language: "markdown".to_string(),
        symbols: vec![symbol],
        calls: Vec::new(),
        imports: Vec::new(),
        trait_impls: Vec::new(),
        refs: Vec::new(),
        parse_status: default_parse_status(),
    })
}

fn node_text(node: TsNode, content: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    if end <= content.len() && start <= end {
        content[start..end].to_string()
    } else {
        String::new()
    }
}

fn node_has_child_kind(node: TsNode, kind: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return true;
        }
    }
    false
}

fn extract_first_line(node: TsNode, content: &str) -> Option<String> {
    let text = node_text(node, content);
    text.lines().next().map(|l| l.trim().to_string())
}

/// JSDoc-style comments directly above a declaration. Decorator lines (`@Component(...)`)
/// between the comment and the declaration are skipped.
fn extract_preceding_docstrings(node: TsNode, lines: &[&str]) -> Option<String> {
    let start_line = node.start_position().row;
    if start_line == 0 || start_line > lines.len() {
        return None;
    }

    let mut doc_lines = Vec::new();
    let mut cur = start_line;

    while cur > 0 {
        cur -= 1;
        let line = lines[cur].trim();
        if line.starts_with("///") || line.starts_with("/**") || line.starts_with('*') {
            doc_lines.push(line);
        } else if (line.starts_with('@') || line.is_empty()) && doc_lines.is_empty() {
            continue;
        } else {
            break;
        }
    }

    if doc_lines.is_empty() {
        None
    } else {
        doc_lines.reverse();
        Some(doc_lines.join("\n"))
    }
}

fn extract_python_docstring(node: TsNode, content: &str) -> Option<String> {
    if let Some(body) = node.child_by_field_name("body") {
        if let Some(first_stmt) = body.named_child(0) {
            if first_stmt.kind() == "expression_statement" {
                if let Some(string_node) = first_stmt.named_child(0) {
                    if string_node.kind() == "string" {
                        return Some(node_text(string_node, content));
                    }
                }
            }
        }
    }
    None
}
