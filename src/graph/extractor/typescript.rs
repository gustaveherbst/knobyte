//! TypeScript / TSX / JavaScript extraction (tree-sitter, source only).
//!
//! The optional type-checker mode (`graph.typescript.compiler: "tsc"`) does not change what is
//! extracted here; it adds checker-resolved facts at graph-build time (see `graph::ts_compiler`).

use super::*;
use std::collections::HashMap;

mod infer;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TsLang {
    TypeScript,
    Tsx,
    JavaScript,
}

#[derive(Default)]
struct TsOut {
    symbols: Vec<ExtractedSymbol>,
    imports: Vec<ExtractedImport>,
    refs: Vec<ExtractedRef>,
    /// Overload signatures: (container, name, header) of bodiless `function f(..): T;` and
    /// class `m(..): T;` declarations preceding an implementation.
    overloads: Vec<(Option<String>, String, String)>,
    /// Enclosing `namespace` names (outermost first): declarations inside are qualified
    /// `NS.inner` so a qualified call `NS.inner()` can be resolved.
    ns: Vec<String>,
    /// Occurrences per anonymous-callback qualified name, for ordinal disambiguation.
    callbacks: HashMap<String, usize>,
}

impl TsOut {
    /// `name` qualified by the enclosing namespaces (`A.B.name`).
    fn qualify(&self, name: &str) -> String {
        if self.ns.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", self.ns.join("."), name)
        }
    }
}

/// Checker-like declaration header: the declaration text before its body, whitespace
/// normalized (`async function load(id: UserId): Promise<User>`), bounded.
fn ts_header(node: TsNode, content: &str) -> Option<String> {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    let start = node.start_byte();
    if end <= start || end > content.len() {
        return None;
    }
    let mut h = content[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
    for suffix in ["=>", ";", "{", "="] {
        h = h.trim_end().trim_end_matches(suffix).trim_end().to_string();
    }
    if h.len() > 400 {
        let mut cut = 400;
        while !h.is_char_boundary(cut) {
            cut -= 1;
        }
        h.truncate(cut);
        h.push('…');
    }
    (!h.is_empty()).then_some(h)
}

/// Attach overload signatures to their implementation: the implementation's signature becomes
/// the overload list (what callers see), its own header kept last.
fn apply_ts_overloads(out: &mut TsOut) {
    if out.overloads.is_empty() {
        return;
    }
    for sym in out.symbols.iter_mut() {
        if !matches!(sym.kind.as_str(), "function" | "method") {
            continue;
        }
        let sigs: Vec<&str> = out
            .overloads
            .iter()
            .filter(|(c, n, _)| n == &sym.name && c.as_deref() == sym.container.as_deref())
            .map(|(_, _, h)| h.as_str())
            .collect();
        if sigs.is_empty() {
            continue;
        }
        let own = sym.signature.clone().unwrap_or_default();
        let mut all: Vec<String> = sigs.iter().map(|s| s.to_string()).collect();
        all.push(own);
        sym.signature = Some(all.join("; "));
    }
}
/// `extends` / `implements` clauses of a class.
fn ts_class_heritage(class: TsNode, content: &str, sym: &ExtractedSymbol, refs: &mut Vec<ExtractedRef>) {
    let mut cursor = class.walk();
    for child in class.children(&mut cursor) {
        if child.kind() != "class_heritage" {
            continue;
        }
        let mut hc = child.walk();
        for part in child.named_children(&mut hc) {
            let kind = match part.kind() {
                "extends_clause" => "extends",
                "implements_clause" => "implements",
                // JavaScript grammar: `class A extends B` puts the expression directly here.
                _ => {
                    if let Some((name, q)) = split_type_ref(&node_text(part, content)) {
                        refs.push(named_ref("extends", sym, &name, q, part));
                    }
                    continue;
                }
            };
            let mut pc = part.walk();
            for v in part.named_children(&mut pc) {
                if v.kind() == "type_arguments" {
                    continue;
                }
                if let Some((name, q)) = split_type_ref(&node_text(v, content)) {
                    refs.push(named_ref(kind, sym, &name, q, v));
                }
            }
        }
    }
}

/// Decorators applied to a declaration: its own `decorator` children plus the contiguous
/// `decorator` siblings directly before it (class members).
fn ts_decorator_refs(node: TsNode, content: &str, sym: &ExtractedSymbol, refs: &mut Vec<ExtractedRef>) {
    let mut decorators: Vec<TsNode> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            decorators.push(child);
        }
    }
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() != "decorator" {
            break;
        }
        decorators.push(p);
        prev = p.prev_named_sibling();
    }
    for d in decorators {
        if let Some(expr) = d.named_child(0) {
            let target = if expr.kind() == "call_expression" {
                expr.child_by_field_name("function")
            } else {
                Some(expr)
            };
            if let Some((name, q)) = target.and_then(|t| split_type_ref(&node_text(t, content))) {
                refs.push(named_ref("decorates", sym, &name, q, d));
            }
        }
    }
}

pub(super) fn extract_ts_like(content: &str, lang: TsLang) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let language = match lang {
        TsLang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        TsLang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        TsLang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
    };
    parser.set_language(&language).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let lines: Vec<&str> = content.lines().collect();

    let mut out = TsOut::default();
    walk_ts_node(root, content, &lines, &mut out, None);
    apply_ts_overloads(&mut out);

    let mut calls = Vec::new();
    collect_ts_calls(root, content, &mut calls, &mut out.refs);
    out.refs.extend(infer::type_facts(root, content));

    Some(ExtractionResult {
        language: match lang {
            TsLang::TypeScript => "typescript",
            TsLang::Tsx => "tsx",
            TsLang::JavaScript => "javascript",
        }
        .to_string(),
        symbols: out.symbols,
        calls,
        imports: out.imports,
        trait_impls: Vec::new(),
        refs: out.refs,
        parse_status: parse_status_of(root),
    })
}

fn is_ts_exported(node: TsNode) -> bool {
    let mut cur = node.parent();
    // Walk through variable_declarator -> lexical_declaration -> export_statement.
    for _ in 0..3 {
        match cur {
            Some(p) if p.kind() == "export_statement" => return true,
            Some(p)
                if p.kind() == "lexical_declaration"
                    || p.kind() == "variable_declaration"
                    || p.kind() == "variable_declarator" =>
            {
                cur = p.parent();
            }
            _ => break,
        }
    }
    node_has_child_kind(node, "export")
}

fn ts_accessibility(node: TsNode, content: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "accessibility_modifier" {
            return Some(node_text(child, content).trim().to_string());
        }
    }
    None
}

fn is_function_value(node: TsNode) -> bool {
    matches!(
        node.kind(),
        "arrow_function" | "function_expression" | "function" | "generator_function"
    )
}

fn walk_ts_children(
    node: TsNode,
    content: &str,
    lines: &[&str],
    out: &mut TsOut,
    container: Option<&str>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ts_node(child, content, lines, out, container);
    }
}

fn walk_ts_node(
    node: TsNode,
    content: &str,
    lines: &[&str],
    out: &mut TsOut,
    container: Option<&str>,
) {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let mut sym = new_symbol(
                    "function",
                    &name,
                    out.qualify(&name),
                    None,
                    node,
                    content,
                    extract_preceding_docstrings(node, lines),
                );
                sym.is_exported = is_ts_exported(node);
                sym.visibility =
                    Some(if sym.is_exported { "public" } else { "module" }.to_string());
                sym.is_async = node_has_child_kind(node, "async");
                sym.return_type = return_type_text(node, content);
                sym.signature = ts_header(node, content).or(sym.signature);
                push_returns(node, content, &sym, &mut out.refs);
                ts_decorator_refs(node, content, &sym, &mut out.refs);
                out.symbols.push(sym);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_ts_children(body, content, lines, out, None);
            }
            return;
        }
        "function_signature" => {
            if let (Some(name), Some(h)) = (field_text(node, "name", content), ts_header(node, content)) {
                out.overloads.push((None, name, h));
            }
            return;
        }
        "method_signature" if container.is_some() => {
            if let (Some(name), Some(h)) = (field_text(node, "name", content), ts_header(node, content)) {
                out.overloads.push((container.map(String::from), name, h));
            }
            return;
        }
        "method_definition" | "abstract_method_signature" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = match container {
                    Some(c) => out.qualify(&format!("{}.{}", c, name)),
                    None => out.qualify(&name),
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
                    extract_preceding_docstrings(node, lines),
                );
                let vis = ts_accessibility(node, content).unwrap_or_else(|| {
                    if name_node.kind() == "private_property_identifier" {
                        "private".to_string()
                    } else {
                        "public".to_string()
                    }
                });
                sym.is_exported = vis == "public";
                sym.visibility = Some(vis);
                sym.is_async = node_has_child_kind(node, "async");
                sym.is_static = node_has_child_kind(node, "static");
                sym.is_abstract = node.kind() == "abstract_method_signature";
                sym.return_type = return_type_text(node, content);
                sym.signature = ts_header(node, content).or(sym.signature);
                push_returns(node, content, &sym, &mut out.refs);
                ts_decorator_refs(node, content, &sym, &mut out.refs);
                out.symbols.push(sym);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_ts_children(body, content, lines, out, None);
            }
            return;
        }
        "public_field_definition" | "field_definition" => {
            let name_node = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("property"));
            let value = node.child_by_field_name("value");
            if let (Some(name_node), None, Some(c)) = (name_node, value, container) {
                ts_property(node, name_node, content, lines, out, c);
            }
            if let (Some(name_node), Some(value), Some(c)) = (name_node, value, container) {
                if !is_function_value(value) {
                    ts_property(node, name_node, content, lines, out, c);
                } else {
                    let name = node_text(name_node, content);
                    let mut sym = new_symbol(
                        "method",
                        &name,
                        out.qualify(&format!("{}.{}", c, name)),
                        Some(c),
                        node,
                        content,
                        extract_preceding_docstrings(node, lines),
                    );
                    let vis =
                        ts_accessibility(node, content).unwrap_or_else(|| "public".to_string());
                    sym.is_exported = vis == "public";
                    sym.visibility = Some(vis);
                    sym.is_async = node_has_child_kind(value, "async");
                    sym.is_static = node_has_child_kind(node, "static");
                    sym.return_type = return_type_text(value, content);
                    if let Some(h) = ts_header(value, content) {
                        sym.signature = Some(format!("{} = {}", name, h));
                    }
                    push_returns(value, content, &sym, &mut out.refs);
                    ts_decorator_refs(node, content, &sym, &mut out.refs);
                    out.symbols.push(sym);
                }
            }
            if let Some(v) = node.child_by_field_name("value") {
                walk_ts_children(v, content, lines, out, None);
            }
            return;
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let mut sym = new_symbol(
                    "class",
                    &name,
                    out.qualify(&name),
                    None,
                    node,
                    content,
                    extract_preceding_docstrings(node, lines),
                );
                sym.is_exported = is_ts_exported(node);
                sym.visibility =
                    Some(if sym.is_exported { "public" } else { "module" }.to_string());
                sym.is_abstract = node.kind() == "abstract_class_declaration";
                sym.signature = ts_header(node, content).or(sym.signature);
                ts_class_heritage(node, content, &sym, &mut out.refs);
                ts_decorator_refs(node, content, &sym, &mut out.refs);
                out.symbols.push(sym);

                if let Some(body_node) = node.child_by_field_name("body") {
                    walk_ts_children(body_node, content, lines, out, Some(&name));
                }
            }
            return;
        }
        "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let kind = match node.kind() {
                    "interface_declaration" => "interface",
                    "type_alias_declaration" => "type_alias",
                    _ => "enum",
                };
                let mut sym = new_symbol(
                    kind,
                    &name,
                    out.qualify(&name),
                    None,
                    node,
                    content,
                    extract_preceding_docstrings(node, lines),
                );
                sym.is_exported = is_ts_exported(node);
                sym.visibility =
                    Some(if sym.is_exported { "public" } else { "module" }.to_string());
                let exported = sym.is_exported;
                sym.signature = ts_header(node, content).or(sym.signature);
                if node.kind() == "type_alias_declaration" {
                    // `type Svc = UserService` / `type Id = Brand<string>`: the alias names one
                    // project type (resolved after extraction into an `aliases` edge).
                    if let Some(value) = node.child_by_field_name("value") {
                        let text = node_text(value, content);
                        if !text.contains(['|', '&', '{', '(']) {
                            if let Some(target) = primary_type_name(&text) {
                                let q = split_type_ref(&text).and_then(|(_, q)| q);
                                out.refs.push(named_ref("aliases", &sym, &target, q, value));
                            }
                        }
                    }
                }
                if node.kind() == "interface_declaration" {
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "extends_type_clause" {
                            let mut tc = child.walk();
                            for t in child.named_children(&mut tc) {
                                if let Some((n, q)) = split_type_ref(&node_text(t, content)) {
                                    out.refs.push(named_ref("extends", &sym, &n, q, t));
                                }
                            }
                        }
                    }
                }
                out.symbols.push(sym);
                if node.kind() == "interface_declaration" {
                    ts_interface_members(node, &name, exported, content, lines, out);
                }
                if node.kind() == "enum_declaration" {
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for m in body.named_children(&mut cursor) {
                            let name_node = match m.kind() {
                                "property_identifier" => Some(m),
                                "enum_assignment" => m.child_by_field_name("name"),
                                _ => None,
                            };
                            if let Some(n) = name_node {
                                let member = strip_quotes(&node_text(n, content));
                                let mut ms = new_symbol(
                                    "enum_member",
                                    &member,
                                    out.qualify(&format!("{}.{}", name, member)),
                                    Some(&name),
                                    m,
                                    content,
                                    None,
                                );
                                ms.is_exported = exported;
                                ms.visibility = Some("public".to_string());
                                out.symbols.push(ms);
                            }
                        }
                    }
                }
            }
            return;
        }
        "internal_module" | "module" => {
            if let (Some(name_node), Some(body)) =
                (node.child_by_field_name("name"), node.child_by_field_name("body"))
            {
                let name = strip_quotes(&node_text(name_node, content));
                let mut sym = new_symbol(
                    "namespace",
                    &name,
                    out.qualify(&name),
                    None,
                    node,
                    content,
                    extract_preceding_docstrings(node, lines),
                );
                sym.is_exported = is_ts_exported(node)
                    || node.parent().is_some_and(|p| p.kind() == "expression_statement"
                        && p.parent().is_some_and(|g| g.kind() == "export_statement"));
                sym.visibility =
                    Some(if sym.is_exported { "public" } else { "module" }.to_string());
                out.symbols.push(sym);
                // `namespace A.B { .. }` qualifies its members `A.B.x`; an ambient module
                // (`declare module "pkg" { .. }`) is not a value path and qualifies nothing.
                let qualifies = name_node.kind() != "string";
                if qualifies {
                    out.ns.push(name);
                }
                walk_ts_children(body, content, lines, out, None);
                if qualifies {
                    out.ns.pop();
                }
                return;
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = node.walk();
            let declarators: Vec<TsNode> = node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "variable_declarator")
                .collect();
            let single = declarators.len() == 1;
            let module_scope = ts_at_module_scope(node);
            let keyword = node
                .child(0)
                .map(|k| node_text(k, content))
                .filter(|k| matches!(k.as_str(), "const" | "let" | "var"))
                .unwrap_or_else(|| "var".to_string());
            for decl in &declarators {
                let name_node = decl.child_by_field_name("name");
                let value = decl.child_by_field_name("value");
                // Module-level `const` / `let` / `var` bindings (not functions, not `require`
                // imports, not destructuring patterns) are `constant` / `variable` nodes.
                if module_scope {
                    if let Some(n) = name_node.filter(|n| n.kind() == "identifier") {
                        if !value.is_some_and(|v| is_function_value(v) || is_require_call(v, content)) {
                            let span = if single { node } else { *decl };
                            ts_module_binding(*decl, span, n, &keyword, content, lines, out);
                        }
                    }
                }
                if let (Some(name_node), Some(value)) = (name_node, value) {
                    if name_node.kind() == "identifier" && is_function_value(value) {
                        let name = node_text(name_node, content);
                        let span = if single { node } else { *decl };
                        let mut sym = new_symbol(
                            "function",
                            &name,
                            out.qualify(&name),
                            None,
                            span,
                            content,
                            extract_preceding_docstrings(span, lines),
                        );
                        sym.is_exported = is_ts_exported(*decl);
                        sym.visibility =
                            Some(if sym.is_exported { "public" } else { "module" }.to_string());
                        sym.is_async = node_has_child_kind(value, "async");
                        sym.return_type = return_type_text(value, content);
                        if let Some(h) = ts_header(value, content) {
                            let kw = node
                                .child(0)
                                .map(|k| node_text(k, content))
                                .filter(|k| matches!(k.as_str(), "const" | "let" | "var"))
                                .unwrap_or_else(|| "const".to_string());
                            sym.signature = Some(format!("{} {} = {}", kw, name, h));
                        }
                        push_returns(value, content, &sym, &mut out.refs);
                        out.symbols.push(sym);
                    }
                    ts_require_import(name_node, value, content, &mut out.imports);
                }
            }
            walk_ts_children(node, content, lines, out, None);
            return;
        }
        "import_statement" => {
            ts_import_statement(node, content, &mut out.imports);
            return;
        }
        "export_statement" => {
            if node.child_by_field_name("source").is_some() {
                ts_reexport(node, content, &mut out.imports);
                return;
            }
            if node_has_child_kind(node, "default") {
                ts_default_export(node, content, out);
            }
        }
        "arrow_function" | "function_expression" | "function" | "generator_function"
            if node.is_named() && !ts_is_named_function_value(node) =>
        {
            ts_anonymous_function(node, content, lines, out);
            return;
        }
        _ => {}
    }

    walk_ts_children(node, content, lines, out, container);
}

/// True when a declaration statement sits at file or namespace scope (optionally exported).
fn ts_at_module_scope(node: TsNode) -> bool {
    let mut parent = node.parent();
    if parent.is_some_and(|p| p.kind() == "export_statement") {
        parent = parent.and_then(|p| p.parent());
    }
    match parent {
        Some(p) if p.kind() == "program" => true,
        Some(p) if p.kind() == "statement_block" => p
            .parent()
            .is_some_and(|g| matches!(g.kind(), "internal_module" | "module")),
        _ => false,
    }
}

fn is_require_call(value: TsNode, content: &str) -> bool {
    value.kind() == "call_expression"
        && value
            .child_by_field_name("function")
            .is_some_and(|f| f.kind() == "identifier" && node_text(f, content) == "require")
}

/// Whitespace-normalized text bounded to `max` bytes (with an ellipsis when cut).
fn bounded_text(text: &str, max: usize) -> String {
    let mut t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.len() > max {
        let mut cut = max;
        while !t.is_char_boundary(cut) {
            cut -= 1;
        }
        t.truncate(cut);
        t.push('…');
    }
    t
}

/// A module-level `const` (`constant`) or `let` / `var` (`variable`) binding.
fn ts_module_binding(
    decl: TsNode,
    span: TsNode,
    name_node: TsNode,
    keyword: &str,
    content: &str,
    lines: &[&str],
    out: &mut TsOut,
) {
    let name = node_text(name_node, content);
    if name.is_empty() {
        return;
    }
    let kind = if keyword == "const" { "constant" } else { "variable" };
    let mut sym = new_symbol(
        kind,
        &name,
        out.qualify(&name),
        None,
        span,
        content,
        extract_preceding_docstrings(span, lines),
    );
    sym.is_exported = is_ts_exported(decl);
    sym.visibility = Some(if sym.is_exported { "public" } else { "module" }.to_string());
    sym.signature = Some(format!("{} {}", keyword, bounded_text(&node_text(decl, content), 200)));
    if let Some(ty) = decl.child_by_field_name("type") {
        if let Some(t) = primary_type_name(&node_text(ty, content)) {
            out.refs.push(decl_ref("type_of", &sym, &t, ty));
        }
    }
    out.symbols.push(sym);
}

/// `export default <declaration | name | function>`: recorded as a `default_export` reference
/// naming the declaration, so `export { default as X } from './m'` and default imports resolve
/// to it (the graph build consumes these references; they never become edges).
fn ts_default_export(node: TsNode, content: &str, out: &mut TsOut) {
    let name = if let Some(decl) = node.child_by_field_name("declaration") {
        field_text(decl, "name", content)
    } else if let Some(value) = node.child_by_field_name("value") {
        match value.kind() {
            "identifier" => Some(node_text(value, content)),
            "arrow_function" | "function_expression" | "function" | "generator_function" => {
                field_text(value, "name", content).or_else(|| Some("default".to_string()))
            }
            "class" => field_text(value, "name", content),
            _ => None,
        }
    } else {
        None
    };
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        out.refs.push(ExtractedRef {
            kind: "default_export".to_string(),
            from: String::new(),
            from_kind: String::new(),
            target_name: name,
            qualifier: None,
            line: node.start_position().row + 1,
            col: node.start_position().column,
        });
    }
}

/// A function value whose name comes from its binding (`const f = () => ..`, a class field, an
/// object property) or from itself (`function named() {}`): not an anonymous callback.
fn ts_is_named_function_value(node: TsNode) -> bool {
    if node.child_by_field_name("name").is_some() {
        return true;
    }
    node.parent().is_some_and(|p| {
        matches!(
            p.kind(),
            "variable_declarator" | "public_field_definition" | "field_definition" | "pair"
        )
    })
}

/// An anonymous function value (a callback argument, a returned closure, an IIFE, an anonymous
/// default export) becomes its own `function` node named `<callback:callee[index]>` (or
/// `<callback:syntax-kind>`), so the calls inside it have a caller of their own; an anonymous
/// default export is named `default`. Same-named callbacks of a file are qualified `#2`, `#3`..
fn ts_anonymous_function(node: TsNode, content: &str, lines: &[&str], out: &mut TsOut) {
    let parent = node.parent();
    let default_export = parent.is_some_and(|p| p.kind() == "export_statement");
    let label = match parent {
        Some(args) if args.kind() == "arguments" => {
            let mut c = args.walk();
            let index = args
                .named_children(&mut c)
                .position(|a| a.id() == node.id())
                .unwrap_or(0);
            let callee = args.parent().and_then(|call| {
                call.child_by_field_name("function")
                    .or_else(|| call.child_by_field_name("constructor"))
            });
            let callee = callee
                .map(|c| bounded_text(&node_text(c, content), 80).replace(' ', ""))
                .unwrap_or_default();
            format!("{}[{}]", callee, index)
        }
        Some(p) => p.kind().to_string(),
        None => "expression".to_string(),
    };
    let name = if default_export {
        "default".to_string()
    } else {
        format!("<callback:{}>", label)
    };
    let base = out.qualify(&name);
    let seen = out.callbacks.entry(base.clone()).or_insert(0);
    *seen += 1;
    let qualified = if *seen == 1 { base } else { format!("{}#{}", base, seen) };
    let mut sym = new_symbol("function", &name, qualified, None, node, content, None);
    if default_export {
        sym.docstring = extract_preceding_docstrings(node, lines);
        sym.is_exported = true;
        sym.visibility = Some("public".to_string());
    } else {
        sym.visibility = Some("local".to_string());
    }
    sym.is_async = node_has_child_kind(node, "async");
    sym.return_type = return_type_text(node, content);
    sym.signature = ts_header(node, content).or(sym.signature);
    push_returns(node, content, &sym, &mut out.refs);
    out.symbols.push(sym);
    walk_ts_children(node, content, lines, out, None);
}

/// A non-function class field (`property`), with a `type_of` reference for its annotation.
/// Interface members: `run(): void` is a `method` and `name: string` a `property`,
/// both contained by the interface, so calls typed by the interface (`const r: Runner; r.run()`)
/// have a declaration to bind to. Overloaded signatures of one method yield one node (the first),
/// whose signature lists every overload.
fn ts_interface_members(
    node: TsNode,
    interface: &str,
    exported: bool,
    content: &str,
    lines: &[&str],
    out: &mut TsOut,
) {
    let Some(body) = node.child_by_field_name("body") else { return };
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let Some(name_node) = member.child_by_field_name("name") else { continue };
        let name = strip_quotes(&node_text(name_node, content));
        if name.is_empty() {
            continue;
        }
        match member.kind() {
            "method_signature" => {
                let header = ts_header(member, content);
                if let Some(&i) = seen.get(&name) {
                    if let (Some(h), Some(sig)) = (header, out.symbols[i].signature.as_mut()) {
                        sig.push('\n');
                        sig.push_str(&h);
                    }
                    continue;
                }
                let mut sym = new_symbol(
                    "method",
                    &name,
                    out.qualify(&format!("{}.{}", interface, name)),
                    Some(interface),
                    member,
                    content,
                    extract_preceding_docstrings(member, lines),
                );
                sym.is_exported = exported;
                sym.visibility = Some("public".to_string());
                sym.return_type = return_type_text(member, content);
                sym.signature = header.or(sym.signature);
                seen.insert(name, out.symbols.len());
                out.symbols.push(sym);
            }
            "property_signature" => {
                if seen.contains_key(&name) {
                    continue;
                }
                seen.insert(name, out.symbols.len());
                ts_property(member, name_node, content, lines, out, interface);
                if let Some(sym) = out.symbols.last_mut() {
                    sym.is_exported = exported;
                    sym.signature = Some(bounded_text(&node_text(member, content), 200));
                }
            }
            _ => {}
        }
    }
}

fn ts_property(
    node: TsNode,
    name_node: TsNode,
    content: &str,
    lines: &[&str],
    out: &mut TsOut,
    container: &str,
) {
    let name = node_text(name_node, content);
    if name.is_empty() {
        return;
    }
    let mut sym = new_symbol(
        "property",
        &name,
        out.qualify(&format!("{}.{}", container, name)),
        Some(container),
        node,
        content,
        extract_preceding_docstrings(node, lines),
    );
    let vis = ts_accessibility(node, content).unwrap_or_else(|| {
        if name_node.kind() == "private_property_identifier" {
            "private".to_string()
        } else {
            "public".to_string()
        }
    });
    sym.is_exported = vis == "public";
    sym.visibility = Some(vis);
    sym.is_static = node_has_child_kind(node, "static");
    if let Some(ty) = node.child_by_field_name("type") {
        if let Some(t) = primary_type_name(&node_text(ty, content)) {
            out.refs.push(decl_ref("type_of", &sym, &t, ty));
        }
    }
    ts_decorator_refs(node, content, &sym, &mut out.refs);
    out.symbols.push(sym);
}

fn ts_import_statement(node: TsNode, content: &str, out: &mut Vec<ExtractedImport>) {
    let source = match node.child_by_field_name("source") {
        Some(s) => strip_quotes(&node_text(s, content)),
        None => {
            // `import x = require('y')`
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "import_require_clause" {
                    let src = field_text(child, "source", content).map(|s| strip_quotes(&s));
                    let mut c2 = child.walk();
                    let local = child
                        .named_children(&mut c2)
                        .find(|n| n.kind() == "identifier")
                        .map(|n| node_text(n, content));
                    if let (Some(src), Some(local)) = (src, local) {
                        out.push(ExtractedImport {
                            imported_name: "*".to_string(),
                            source_module: src,
                            local_name: local,
                            is_module: true,
                            is_type_only: false,
                            is_reexport: false,
                            line: node.start_position().row + 1,
                        });
                    }
                }
            }
            return;
        }
    };
    let line = node.start_position().row + 1;
    let type_only = node_has_child_kind(node, "type");
    let mut bound = false;

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "import_clause" => {
                let mut c2 = child.walk();
                for part in child.named_children(&mut c2) {
                    match part.kind() {
                        "identifier" => {
                            bound = true;
                            out.push(ExtractedImport {
                                imported_name: "default".to_string(),
                                source_module: source.clone(),
                                local_name: node_text(part, content),
                                is_module: false,
                                is_type_only: type_only,
                                is_reexport: false,
                                line,
                            });
                        }
                        "namespace_import" => {
                            let mut c3 = part.walk();
                            let id_node = part
                                .named_children(&mut c3)
                                .find(|n| n.kind() == "identifier");
                            if let Some(id) = id_node {
                                bound = true;
                                out.push(ExtractedImport {
                                    imported_name: "*".to_string(),
                                    source_module: source.clone(),
                                    local_name: node_text(id, content),
                                    is_module: true,
                                    is_type_only: type_only,
                                    is_reexport: false,
                                    line,
                                });
                            }
                        }
                        "named_imports" => {
                            let mut c3 = part.walk();
                            for spec in part.named_children(&mut c3) {
                                if spec.kind() != "import_specifier" {
                                    continue;
                                }
                                if let Some(name) = field_text(spec, "name", content) {
                                    let name = strip_quotes(&name);
                                    let local = field_text(spec, "alias", content)
                                        .unwrap_or_else(|| name.clone());
                                    bound = true;
                                    out.push(ExtractedImport {
                                        imported_name: name,
                                        source_module: source.clone(),
                                        local_name: local,
                                        is_module: false,
                                        is_type_only: type_only
                                            || node_has_child_kind(spec, "type"),
                                        is_reexport: false,
                                        line,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "import_require_clause" => {}
            _ => {}
        }
    }

    if !bound {
        // Side-effect import: `import './polyfills'`
        out.push(ExtractedImport {
            imported_name: String::new(),
            source_module: source,
            local_name: String::new(),
            is_module: true,
            is_type_only: type_only,
            is_reexport: false,
            line,
        });
    }
}

fn ts_reexport(node: TsNode, content: &str, out: &mut Vec<ExtractedImport>) {
    let source = match field_text(node, "source", content) {
        Some(s) => strip_quotes(&s),
        None => return,
    };
    let line = node.start_position().row + 1;
    let mut found = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "export_clause" => {
                let mut c2 = child.walk();
                for spec in child.named_children(&mut c2) {
                    if spec.kind() != "export_specifier" {
                        continue;
                    }
                    if let Some(name) = field_text(spec, "name", content) {
                        let name = strip_quotes(&name);
                        let local = field_text(spec, "alias", content)
                            .map(|a| strip_quotes(&a))
                            .unwrap_or_else(|| name.clone());
                        found = true;
                        out.push(ExtractedImport {
                            imported_name: name,
                            source_module: source.clone(),
                            local_name: local,
                            is_module: false,
                            is_type_only: false,
                            is_reexport: true,
                            line,
                        });
                    }
                }
            }
            "namespace_export" => {
                let mut c2 = child.walk();
                let local = child
                    .named_children(&mut c2)
                    .next()
                    .map(|n| strip_quotes(&node_text(n, content)))
                    .unwrap_or_default();
                found = true;
                out.push(ExtractedImport {
                    imported_name: "*".to_string(),
                    source_module: source.clone(),
                    local_name: local,
                    is_module: true,
                    is_type_only: false,
                    is_reexport: true,
                    line,
                });
            }
            _ => {}
        }
    }
    if !found {
        // `export * from './x'`
        out.push(ExtractedImport {
            imported_name: "*".to_string(),
            source_module: source,
            local_name: String::new(),
            is_module: true,
            is_type_only: false,
            is_reexport: true,
            line,
        });
    }
}

/// `const x = require('y')` / `const { a, b: c } = require('y')`
fn ts_require_import(
    name_node: TsNode,
    value: TsNode,
    content: &str,
    out: &mut Vec<ExtractedImport>,
) {
    if value.kind() != "call_expression" {
        return;
    }
    let func = match value.child_by_field_name("function") {
        Some(f) => f,
        None => return,
    };
    if func.kind() != "identifier" || node_text(func, content) != "require" {
        return;
    }
    let source = value.child_by_field_name("arguments").and_then(|args| {
        let mut c = args.walk();
        let first = args.named_children(&mut c).next();
        first
            .filter(|a| a.kind() == "string")
            .map(|a| strip_quotes(&node_text(a, content)))
    });
    let source = match source {
        Some(s) => s,
        None => return,
    };
    let line = value.start_position().row + 1;
    match name_node.kind() {
        "identifier" => out.push(ExtractedImport {
            imported_name: "*".to_string(),
            source_module: source,
            local_name: node_text(name_node, content),
            is_module: true,
            is_type_only: false,
            is_reexport: false,
            line,
        }),
        "object_pattern" => {
            let mut c = name_node.walk();
            for prop in name_node.named_children(&mut c) {
                let (imported, local) = match prop.kind() {
                    "shorthand_property_identifier_pattern" => {
                        let n = node_text(prop, content);
                        (n.clone(), n)
                    }
                    "pair_pattern" => {
                        let key = field_text(prop, "key", content).unwrap_or_default();
                        let val = field_text(prop, "value", content).unwrap_or_default();
                        (strip_quotes(&key), val)
                    }
                    _ => continue,
                };
                if imported.is_empty() {
                    continue;
                }
                out.push(ExtractedImport {
                    imported_name: imported,
                    source_module: source.clone(),
                    local_name: local,
                    is_module: false,
                    is_type_only: false,
                    is_reexport: false,
                    line,
                });
            }
        }
        _ => {}
    }
}

fn collect_ts_calls(
    node: TsNode,
    content: &str,
    calls: &mut Vec<ExtractedCall>,
    refs: &mut Vec<ExtractedRef>,
) {
    match node.kind() {
        "call_expression" => {
            if let Some(args) = node.child_by_field_name("arguments") {
                collect_identifier_args(args, content, refs);
            }
            if let Some(func) = node.child_by_field_name("function") {
                let parsed = match func.kind() {
                    "identifier" => {
                        let name = node_text(func, content);
                        if name == "require" {
                            None
                        } else {
                            Some((name, None, false, None))
                        }
                    }
                    "member_expression" => {
                        let prop = field_text(func, "property", content);
                        let object = func.child_by_field_name("object");
                        let obj = object.map(|o| node_text(o, content).trim().to_string());
                        // The receiver's type, as far as it can be inferred without a checker.
                        let receiver_type = object.and_then(|o| infer::receiver_expr(o, content));
                        prop.map(|p| (p, obj, true, receiver_type))
                    }
                    _ => None,
                };
                if let Some((target, receiver, is_method, receiver_type)) = parsed {
                    if !target.is_empty() {
                        calls.push(ExtractedCall {
                            caller_name: String::new(),
                            target_name: target,
                            receiver,
                            qualifier: None,
                            receiver_type,
                            is_method,
                            line: node.start_position().row + 1,
                            col: node.start_position().column,
                        });
                    }
                }
            }
        }
        "new_expression" => {
            if let Some(ctor) = node.child_by_field_name("constructor") {
                // `new ns.Thing()`: a class reached through a namespace import.
                if ctor.kind() == "member_expression" {
                    let obj = ctor.child_by_field_name("object").filter(|o| o.kind() == "identifier");
                    let prop = ctor.child_by_field_name("property");
                    if let (Some(obj), Some(prop)) = (obj, prop) {
                        calls.push(ExtractedCall {
                            caller_name: String::new(),
                            target_name: node_text(prop, content),
                            receiver: None,
                            qualifier: Some(node_text(obj, content)),
                            receiver_type: None,
                            is_method: false,
                            line: node.start_position().row + 1,
                            col: node.start_position().column,
                        });
                    }
                }
                if ctor.kind() == "identifier" {
                    calls.push(ExtractedCall {
                        caller_name: String::new(),
                        target_name: node_text(ctor, content),
                        receiver: None,
                        qualifier: None,
                        receiver_type: None,
                        is_method: false,
                        line: node.start_position().row + 1,
                        col: node.start_position().column,
                    });
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_ts_calls(child, content, calls, refs);
    }
}

