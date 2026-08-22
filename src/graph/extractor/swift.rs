//! Swift extraction (tree-sitter-swift).
//!
//! Emits classes and actors (`class`), structs, enums (+ cases as `enum_member`), protocols
//! (`interface`, with their requirements as abstract members), typealiases and associated types
//! (`type_alias`), functions, methods, initializers (`init`), deinitializers (`deinit`),
//! subscripts (`subscript`), properties (stored and computed, instance and static), global
//! constants (`let`) and variables (`var`) and parameters, plus `import` / `@testable import`
//! module imports, calls, initializer calls (`T(..)`, resolved to `instantiates`) and
//! inheritance.
//!
//! Extensions are not nodes: their members belong to the extended type (`container` is its
//! name), and `extension T: P` records `implements` from `T` (a reference whose `from_kind` is
//! `extension`, so the build resolves `T` wherever it is declared). A class inheritance clause
//! does not say which entry is the superclass: the first entry is `extends` (the build turns it
//! into `implements` when it lands on a protocol) and the rest `implements`; every entry of a
//! struct / enum / actor is `implements` and of a protocol `extends`.
//!
//! Calls carry their argument labels in the target name (`move(to:by:)`, `*` for a trailing
//! closure) and overloads are qualified with their selector (`Shape.move(to:by:)`), so the
//! resolver can pick the overload a call names. Receiver types are inferred from
//! `let x = T(..)`, `let x: T`, parameter and property types, and `super` (the class's first
//! inherited type).

use std::collections::HashMap;

use super::*;

/// Standard-library, Foundation and SwiftUI names that are never project declarations; skipping
/// them keeps conformance lists (`: Codable, Hashable`) out of the unresolved references.
const SWIFT_BUILTIN_TYPES: &[&str] = &[
    "Any", "AnyObject", "AnyHashable", "Array", "Bool", "Character", "CaseIterable", "Codable",
    "Comparable", "CustomDebugStringConvertible", "CustomStringConvertible", "Data", "Date",
    "Decodable", "Dictionary", "Double", "Encodable", "Equatable", "Error", "Float", "Hashable",
    "Identifiable", "Int", "Int8", "Int16", "Int32", "Int64", "LocalizedError", "Never",
    "NSObject", "Optional", "ObservableObject", "RawRepresentable", "Result", "Self", "Sendable",
    "Set", "String", "Substring", "UInt", "UInt8", "UInt16", "UInt32", "UInt64", "URL", "UUID",
    "View", "Void",
];

/// Generic wrappers whose element type is the interesting one (`[T]`, `Set<T>`, `T?`).
const SWIFT_WRAPPERS: &[&str] = &["Array", "Optional", "Set", "ContiguousArray", "Published", "Binding", "State"];

#[derive(Default)]
struct TypeCtx {
    /// Simple name (the resolver's `container` key).
    name: String,
    /// First inherited type of a class: what `super` names.
    superclass: Option<String>,
    /// Property name -> project type name.
    props: HashMap<String, String>,
    /// Default visibility for members (`public extension`).
    member_visibility: Option<String>,
}

#[derive(Default)]
struct SwOut {
    symbols: Vec<ExtractedSymbol>,
    imports: Vec<ExtractedImport>,
    refs: Vec<ExtractedRef>,
    calls: Vec<ExtractedCall>,
    /// Qualified-name prefix (enclosing types and functions), outermost first.
    scope: Vec<String>,
    types: Vec<TypeCtx>,
    /// Local variable / parameter name -> type, per enclosing callable (innermost last).
    locals: Vec<HashMap<String, String>>,
    /// (symbol index, selector) of every callable, for overload disambiguation.
    selectors: Vec<(usize, String)>,
    /// File-level constant / variable name -> type (receivers in top-level code).
    globals: HashMap<String, String>,
}

impl SwOut {
    fn qualify(&self, name: &str) -> String {
        if self.scope.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", self.scope.join("."), name)
        }
    }
    fn container(&self) -> Option<String> {
        self.types.last().map(|t| t.name.clone())
    }
    fn local_type(&self, name: &str) -> Option<String> {
        self.locals
            .iter()
            .rev()
            .find_map(|m| m.get(name).cloned())
            .or_else(|| self.globals.get(name).cloned())
    }
}

pub(super) fn extract_swift(content: &str) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = tree_sitter_swift::LANGUAGE.into();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let mut out = SwOut::default();
    let mut c = root.walk();
    for child in root.named_children(&mut c) {
        visit(child, content, &mut out);
    }
    disambiguate_overloads(&mut out);
    // Generic parameters (`T`, `Element`) are not project types.
    let mut generics = std::collections::HashSet::new();
    collect_generic_params(root, content, &mut generics);
    out.refs.retain(|r| {
        !(matches!(r.kind.as_str(), "returns" | "type_of" | "aliases") && generics.contains(&r.target_name))
    });
    Some(ExtractionResult {
        language: "swift".to_string(),
        symbols: out.symbols,
        calls: out.calls,
        imports: out.imports,
        trait_impls: Vec::new(),
        refs: out.refs,
        parse_status: parse_status_of(root),
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut c = node.walk();
    node.named_children(&mut c).collect()
}

fn child_of_kind<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut c = node.walk();
    let found = node.named_children(&mut c).find(|ch| ch.kind() == kind);
    found
}

/// Modifier keywords (`public`, `static`, `override`, `final`, ..) and attribute names
/// (`@MainActor` -> `@MainActor`).
fn modifiers(node: TsNode, content: &str) -> Vec<String> {
    let Some(m) = child_of_kind(node, "modifiers") else { return Vec::new() };
    named_children(m)
        .into_iter()
        .map(|ch| {
            let t = node_text(ch, content);
            if ch.kind() == "attribute" {
                t.split('(').next().unwrap_or("").trim().to_string()
            } else {
                t.split_whitespace().collect::<Vec<_>>().join(" ")
            }
        })
        .collect()
}

fn visibility_of(mods: &[String]) -> Option<String> {
    ["open", "public", "package", "internal", "fileprivate", "private"]
        .iter()
        .find(|v| mods.iter().any(|m| m == *v || m.starts_with(&format!("{}(", v))))
        .map(|v| v.to_string())
}

/// Declaration header up to (not including) its body, on one line, without attributes.
fn header(node: TsNode, content: &str) -> Option<String> {
    let start = node.start_byte();
    let end = node
        .child_by_field_name("body")
        .or_else(|| child_of_kind(node, "computed_property"))
        .or_else(|| child_of_kind(node, "function_body"))
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    let text = content.get(start..end)?;
    let words: Vec<&str> = text
        .split_whitespace()
        .skip_while(|w| w.starts_with('@'))
        .collect();
    let h = words.join(" ");
    let h = h.trim_end_matches('{').trim().to_string();
    (!h.is_empty()).then_some(h)
}

/// `///` (and `/** */`) documentation directly above a declaration; attribute lines between
/// the comment and the declaration are skipped.
fn docs(node: TsNode, content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut row = node.start_position().row;
    let mut doc = Vec::new();
    let mut in_block = false;
    while row > 0 {
        row -= 1;
        let l = lines.get(row).map(|l| l.trim()).unwrap_or("");
        if in_block {
            let t = l.trim_start_matches("/**").trim_start_matches('*').trim();
            if !t.is_empty() {
                doc.push(t.to_string());
            }
            if l.starts_with("/**") {
                break;
            }
        } else if let Some(t) = l.strip_prefix("///") {
            doc.push(t.trim().to_string());
        } else if l.ends_with("*/") && doc.is_empty() {
            in_block = true;
            let t = l.trim_end_matches("*/").trim_start_matches("/**").trim_start_matches('*').trim();
            if !t.is_empty() {
                doc.push(t.to_string());
            }
            if l.starts_with("/**") {
                break;
            }
        } else if l.starts_with('@') && doc.is_empty() {
            continue;
        } else {
            break;
        }
    }
    if doc.is_empty() {
        return None;
    }
    doc.reverse();
    let text = doc.join(" ").trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The project type a Swift type annotation names: `[User]` -> `User`, `User?` -> `User`,
/// `some Shape` -> `Shape`, `Set<Tag>` -> `Tag`, `Core.User` -> `User`. Builtins, function and
/// tuple types yield `None`.
fn swift_type_name(text: &str) -> Option<String> {
    let mut t = text.trim().trim_start_matches(':').trim().to_string();
    loop {
        let before = t.clone();
        for p in ["some ", "any ", "inout ", "borrowing ", "consuming ", "sending "] {
            if let Some(r) = t.strip_prefix(p) {
                t = r.trim().to_string();
            }
        }
        while t.starts_with('@') {
            t = t.split_once(' ').map(|(_, r)| r.trim().to_string()).unwrap_or_default();
        }
        t = t.trim_end_matches(['?', '!']).to_string();
        if t.starts_with('[') && t.ends_with(']') {
            let inner = &t[1..t.len() - 1];
            // `[K: V]` -> V
            t = inner.rsplit_once(':').map(|(_, v)| v).unwrap_or(inner).trim().to_string();
        }
        if let Some(open) = t.find('<') {
            let base = t[..open].rsplit('.').next().unwrap_or("").to_string();
            if SWIFT_WRAPPERS.contains(&base.as_str()) && t.ends_with('>') {
                t = t[open + 1..t.len() - 1].trim().to_string();
            }
        }
        if t == before {
            break;
        }
    }
    if t.is_empty() || t.contains(['(', '&', ',', ' ']) {
        return None;
    }
    let base = strip_generics(&t);
    let name = base.rsplit('.').next().unwrap_or(&base).to_string();
    if name.is_empty()
        || !name.chars().next().is_some_and(char::is_uppercase)
        || !name.chars().all(|c| c.is_alphanumeric() || c == '_')
        || SWIFT_BUILTIN_TYPES.contains(&name.as_str())
    {
        return None;
    }
    Some(name)
}

/// The type node of a parameter / type annotation: its last named child that is a type.
fn type_child(node: TsNode) -> Option<TsNode> {
    named_children(node).into_iter().rev().find(|ch| {
        !matches!(
            ch.kind(),
            "simple_identifier" | "parameter_modifiers" | "modifiers" | "attribute" | "comment"
        )
    })
}

/// Return type of a function-like declaration: the named node after `->`.
fn return_type_node(node: TsNode) -> Option<TsNode> {
    let mut c = node.walk();
    let mut after_arrow = false;
    for ch in node.children(&mut c) {
        if after_arrow && ch.is_named() {
            return Some(ch);
        }
        if ch.kind() == "->" {
            after_arrow = true;
        }
    }
    None
}

fn collect_generic_params(node: TsNode, content: &str, out: &mut std::collections::HashSet<String>) {
    if node.kind() == "type_parameter" {
        if let Some(id) = child_of_kind(node, "type_identifier") {
            out.insert(node_text(id, content));
        }
    }
    for ch in named_children(node) {
        collect_generic_params(ch, content, out);
    }
}

/// `override` member of a class: an `overrides` reference to the superclass member it names
/// (`init(name:)` for callables, so the resolver can pick the overload).
fn push_override(mods: &[String], sym: &ExtractedSymbol, target: &str, at: TsNode, out: &mut SwOut) {
    if !mods.iter().any(|m| m == "override") {
        return;
    }
    if let Some(sup) = out.types.last().and_then(|t| t.superclass.clone()) {
        out.refs.push(named_ref("overrides", sym, target, Some(sup), at));
    }
}

fn has_token(node: TsNode, token: &str) -> bool {
    let mut c = node.walk();
    let found = node.children(&mut c).any(|ch| ch.kind() == token);
    found
}

fn skip_inherited(name: &str) -> bool {
    SWIFT_BUILTIN_TYPES.contains(&name)
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

fn visit(node: TsNode, content: &str, out: &mut SwOut) {
    match node.kind() {
        "import_declaration" => import_decl(node, content, out),
        "class_declaration" => type_decl(node, content, out),
        "protocol_declaration" => protocol_decl(node, content, out),
        "function_declaration" | "protocol_function_declaration" | "init_declaration" | "deinit_declaration"
        | "subscript_declaration" => function_decl(node, content, out),
        "property_declaration" | "protocol_property_declaration" => property_decl(node, content, out),
        "typealias_declaration" | "associatedtype_declaration" => typealias_decl(node, content, out),
        "enum_entry" => enum_entry(node, content, out),
        "comment" | "multiline_comment" => {}
        // A freestanding macro (`#Preview { ... }`): its closure body is local code, so its
        // declarations are locals, while its calls are still recorded.
        "macro_invocation" => {
            out.locals.push(HashMap::new());
            walk_body(node, content, out);
            out.locals.pop();
        }
        // Top-level statements (`main.swift`, scripts).
        _ => walk_body(node, content, out),
    }
}

fn import_decl(node: TsNode, content: &str, out: &mut SwOut) {
    let Some(id) = child_of_kind(node, "identifier") else { return };
    let module = node_text(id, content).split_whitespace().collect::<String>();
    if module.is_empty() {
        return;
    }
    let testable = modifiers(node, content).iter().any(|m| m == "@testable");
    let first = module.split('.').next().unwrap_or(&module).to_string();
    out.imports.push(ExtractedImport {
        imported_name: if testable { "@testable".to_string() } else { "*".to_string() },
        source_module: module,
        local_name: first,
        is_module: true,
        is_type_only: false,
        is_reexport: modifiers(node, content).iter().any(|m| m == "@_exported"),
        line: node.start_position().row + 1,
    });
}

/// Inherited types of a declaration, in order: (name, qualifier, specifier node).
fn inheritance_entries<'a>(node: TsNode<'a>, content: &str) -> Vec<(String, Option<String>, TsNode<'a>)> {
    named_children(node)
        .into_iter()
        .filter(|ch| ch.kind() == "inheritance_specifier")
        .filter_map(|spec| {
            let t = spec.child_by_field_name("inherits_from").unwrap_or(spec);
            let (name, q) = split_type_ref(&node_text(t, content))?;
            Some((name, q, spec))
        })
        .collect()
}

fn type_decl(node: TsNode, content: &str, out: &mut SwOut) {
    let decl_kind = node
        .child_by_field_name("declaration_kind")
        .map(|k| k.kind().to_string())
        .unwrap_or_else(|| "class".to_string());
    let Some(name_node) = node.child_by_field_name("name") else { return };
    let full_name = strip_generics(&node_text(name_node, content));
    let mods = modifiers(node, content);
    let entries = inheritance_entries(node, content);

    if decl_kind == "extension" {
        let name = full_name.rsplit('.').next().unwrap_or(&full_name).to_string();
        if name.is_empty() {
            return;
        }
        for (target, q, at) in &entries {
            if skip_inherited(target) {
                continue;
            }
            out.refs.push(ExtractedRef {
                kind: "implements".to_string(),
                from: name.clone(),
                from_kind: "extension".to_string(),
                target_name: target.clone(),
                qualifier: q.clone(),
                line: at.start_position().row + 1,
                col: at.start_position().column,
            });
        }
        let mut ctx = TypeCtx {
            name: name.clone(),
            member_visibility: visibility_of(&mods),
            ..Default::default()
        };
        collect_props(node, content, &mut ctx);
        // Members are qualified by the extended type, not by where the extension sits.
        let saved = std::mem::take(&mut out.scope);
        out.scope = full_name.split('.').map(String::from).collect();
        out.types.push(ctx);
        walk_members(node, content, out);
        out.types.pop();
        out.scope = saved;
        return;
    }

    let kind = match decl_kind.as_str() {
        "struct" => "struct",
        "enum" => "enum",
        _ => "class",
    };
    let name = full_name;
    let mut sym = new_symbol(kind, &name, out.qualify(&name), out.container().as_deref(), node, content, docs(node, content));
    sym.visibility = visibility_of(&mods).or_else(|| Some("internal".into()));
    sym.is_exported = matches!(sym.visibility.as_deref(), Some("public" | "open"));
    sym.is_static = true;
    sym.signature = header(node, content);
    let mut superclass = None;
    for (i, (target, q, at)) in entries.iter().enumerate() {
        let rel = if kind == "class" && i == 0 { "extends" } else { "implements" };
        if rel == "extends" && decl_kind == "class" {
            superclass = Some(target.clone());
        }
        if skip_inherited(target) {
            continue;
        }
        out.refs.push(named_ref(rel, &sym, target, q.clone(), *at));
    }
    out.symbols.push(sym);
    let mut ctx = TypeCtx {
        name: name.clone(),
        superclass,
        ..Default::default()
    };
    collect_props(node, content, &mut ctx);
    out.scope.push(name);
    out.types.push(ctx);
    walk_members(node, content, out);
    out.types.pop();
    out.scope.pop();
}

fn protocol_decl(node: TsNode, content: &str, out: &mut SwOut) {
    let Some(name) = field_text(node, "name", content) else { return };
    let mods = modifiers(node, content);
    let mut sym = new_symbol("interface", &name, out.qualify(&name), out.container().as_deref(), node, content, docs(node, content));
    sym.visibility = visibility_of(&mods).or_else(|| Some("internal".into()));
    sym.is_exported = matches!(sym.visibility.as_deref(), Some("public" | "open"));
    sym.is_abstract = true;
    sym.signature = header(node, content);
    for (target, q, at) in inheritance_entries(node, content) {
        if !skip_inherited(&target) {
            out.refs.push(named_ref("extends", &sym, &target, q, at));
        }
    }
    let vis = sym.visibility.clone();
    out.symbols.push(sym);
    let mut ctx = TypeCtx {
        name: name.clone(),
        member_visibility: vis,
        ..Default::default()
    };
    collect_props(node, content, &mut ctx);
    out.scope.push(name);
    out.types.push(ctx);
    walk_members(node, content, out);
    out.types.pop();
    out.scope.pop();
}

fn walk_members(node: TsNode, content: &str, out: &mut SwOut) {
    if let Some(body) = node.child_by_field_name("body") {
        for m in named_children(body) {
            visit(m, content, out);
        }
    }
}

/// Property name -> type of a type body's stored and computed properties (so `prop.m()` and
/// `self.prop.m()` know their receiver type).
fn collect_props(node: TsNode, content: &str, ctx: &mut TypeCtx) {
    let Some(body) = node.child_by_field_name("body") else { return };
    for m in named_children(body) {
        if !matches!(m.kind(), "property_declaration" | "protocol_property_declaration") {
            continue;
        }
        for (name, ty) in property_bindings(m, content) {
            if let Some(t) = ty {
                ctx.props.insert(name, t);
            }
        }
    }
}

/// (name, inferred project type) of each binding of a property declaration: the type
/// annotation, else an initializer call `T(..)` / `T.init(..)`.
fn property_bindings(node: TsNode, content: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let annotated = child_of_kind(node, "type_annotation")
        .and_then(type_child)
        .and_then(|t| swift_type_name(&node_text(t, content)));
    let mut c = node.walk();
    let mut current: Option<String> = None;
    for (i, ch) in node.children(&mut c).enumerate() {
        match node.field_name_for_child(i as u32) {
            Some("name") if ch.kind() == "pattern" => {
                if let Some(n) = current.take() {
                    out.push((n, annotated.clone()));
                }
                let n = ch
                    .child_by_field_name("bound_identifier")
                    .map(|b| node_text(b, content))
                    .unwrap_or_else(|| node_text(ch, content));
                current = Some(n);
            }
            Some("value") => {
                if let Some(n) = current.take() {
                    out.push((n, annotated.clone().or_else(|| initializer_type(ch, content))));
                }
            }
            _ => {}
        }
    }
    if let Some(n) = current {
        out.push((n, annotated));
    }
    out
}

/// `T(..)`, `T.init(..)`, `try T(..)`, `await T(..)` -> `T`.
fn initializer_type(value: TsNode, content: &str) -> Option<String> {
    let mut v = value;
    while matches!(v.kind(), "try_expression" | "await_expression") {
        v = v.named_child(v.named_child_count().saturating_sub(1))?;
    }
    if v.kind() != "call_expression" {
        return None;
    }
    let callee = v.named_child(0)?;
    let text = match callee.kind() {
        "simple_identifier" => node_text(callee, content),
        "navigation_expression" => {
            let suffix = callee.child_by_field_name("suffix").map(|s| node_text(s, content))?;
            if suffix.trim_start_matches('.') != "init" {
                return None;
            }
            node_text(callee.child_by_field_name("target")?, content)
        }
        _ => return None,
    };
    swift_type_name(&text)
}

fn enum_entry(node: TsNode, content: &str, out: &mut SwOut) {
    let Some(owner) = out.container() else { return };
    let q = out.scope.join(".");
    let mut c = node.walk();
    let names: Vec<TsNode> = node
        .children(&mut c)
        .enumerate()
        .filter(|(i, _)| node.field_name_for_child(*i as u32) == Some("name"))
        .map(|(_, ch)| ch)
        .collect();
    for n in names {
        let name = node_text(n, content);
        let mut sym = new_symbol("enum_member", &name, format!("{}.{}", q, name), Some(&owner), n, content, docs(node, content));
        sym.signature = Some(format!("case {}", name));
        sym.is_exported = true;
        out.symbols.push(sym);
    }
}

fn typealias_decl(node: TsNode, content: &str, out: &mut SwOut) {
    let Some(name_node) = node.child_by_field_name("name") else { return };
    let name = strip_generics(&node_text(name_node, content));
    let mods = modifiers(node, content);
    let mut sym = new_symbol("type_alias", &name, out.qualify(&name), out.container().as_deref(), node, content, docs(node, content));
    sym.visibility = visibility_of(&mods)
        .or_else(|| out.types.last().and_then(|t| t.member_visibility.clone()))
        .or_else(|| Some("internal".into()));
    sym.is_exported = matches!(sym.visibility.as_deref(), Some("public" | "open"));
    sym.signature = Some(node_text(node, content).split_whitespace().collect::<Vec<_>>().join(" "));
    sym.is_abstract = node.kind() == "associatedtype_declaration";
    // The aliased type: the second `name` field (`typealias Id = User`).
    let mut c = node.walk();
    let value = node
        .children(&mut c)
        .enumerate()
        .filter(|(i, ch)| node.field_name_for_child(*i as u32) == Some("name") && ch.id() != name_node.id())
        .map(|(_, ch)| ch)
        .next();
    if let Some(t) = value.and_then(|v| swift_type_name(&node_text(v, content))) {
        sym.return_type = value.map(|v| node_text(v, content));
        out.refs.push(decl_ref("aliases", &sym, &t, node));
    }
    out.symbols.push(sym);
}

/// (external label, internal name, type node) of each parameter.
fn params<'a>(node: TsNode<'a>, content: &str) -> Vec<(String, String, Option<TsNode<'a>>)> {
    named_children(node)
        .into_iter()
        .filter(|ch| ch.kind() == "parameter")
        .filter_map(|p| {
            let external = p.child_by_field_name("external_name").map(|e| node_text(e, content));
            let internal = named_children(p)
                .into_iter()
                .find(|ch| ch.kind() == "simple_identifier" && Some(ch.id()) != p.child_by_field_name("external_name").map(|e| e.id()))
                .map(|ch| node_text(ch, content))?;
            let label = external.unwrap_or_else(|| internal.clone());
            Some((label, internal, type_child(p)))
        })
        .collect()
}

fn function_decl(node: TsNode, content: &str, out: &mut SwOut) {
    let name = match node.kind() {
        "init_declaration" => "init".to_string(),
        "deinit_declaration" => "deinit".to_string(),
        "subscript_declaration" => "subscript".to_string(),
        _ => match node.child_by_field_name("name") {
            Some(n) => node_text(n, content),
            None => return,
        },
    };
    let mods = modifiers(node, content);
    let in_type = !out.types.is_empty() && out.locals.is_empty();
    let container = if in_type { out.container() } else { None };
    let qualified = out.qualify(&name);
    let mut sym = new_symbol(
        if in_type { "method" } else { "function" },
        &name,
        qualified,
        container.as_deref(),
        node,
        content,
        docs(node, content),
    );
    sym.visibility = visibility_of(&mods)
        .or_else(|| if in_type { out.types.last().and_then(|t| t.member_visibility.clone()) } else { None })
        .or_else(|| Some("internal".into()));
    sym.is_exported = matches!(sym.visibility.as_deref(), Some("public" | "open"));
    sym.is_static = mods.iter().any(|m| m == "static" || m == "class");
    sym.is_abstract = node.kind() == "protocol_function_declaration"
        || (node.child_by_field_name("body").is_none()
            && child_of_kind(node, "function_body").is_none()
            && child_of_kind(node, "computed_property").is_none());
    sym.is_async = has_token(node, "async");
    sym.signature = header(node, content);
    let rt = return_type_node(node);
    sym.return_type = rt.map(|t| node_text(t, content));
    if let Some(t) = rt.and_then(|t| swift_type_name(&node_text(t, content))) {
        out.refs.push(decl_ref("returns", &sym, &t, node));
    }
    let ps = params(node, content);
    let selector = format!(
        "{}({})",
        name,
        ps.iter().map(|(l, _, _)| format!("{}:", l)).collect::<String>()
    );
    let mut locals = HashMap::new();
    let mut param_syms = Vec::new();
    for (_, internal, ty) in &ps {
        let Some(pnode) = ty.and_then(|t| t.parent()) else { continue };
        let mut p = new_symbol("parameter", internal, format!("{}.{}", sym.qualified_name, internal), None, pnode, content, None);
        p.return_type = ty.map(|t| node_text(t, content));
        p.signature = Some(node_text(pnode, content).split_whitespace().collect::<Vec<_>>().join(" "));
        if let Some(t) = ty.and_then(|t| swift_type_name(&node_text(t, content))) {
            out.refs.push(decl_ref("type_of", &p, &t, pnode));
            locals.insert(internal.clone(), t);
        }
        param_syms.push(p);
    }
    push_override(&mods, &sym, &selector, node, out);
    out.selectors.push((out.symbols.len(), selector));
    out.symbols.push(sym);
    out.symbols.extend(param_syms);
    let body = node
        .child_by_field_name("body")
        .or_else(|| child_of_kind(node, "function_body"))
        .or_else(|| child_of_kind(node, "computed_property"));
    if let Some(body) = body {
        out.scope.push(name);
        out.locals.push(locals);
        walk_body(body, content, out);
        out.locals.pop();
        out.scope.pop();
    }
}

fn property_decl(node: TsNode, content: &str, out: &mut SwOut) {
    let mods = modifiers(node, content);
    let is_let = child_of_kind(node, "value_binding_pattern")
        .map(|v| node_text(v, content).trim() == "let")
        .or_else(|| {
            node.child_by_field_name("name")
                .and_then(|p| child_of_kind(p, "value_binding_pattern"))
                .map(|v| node_text(v, content).trim() == "let")
        })
        .unwrap_or(false);
    let bindings = property_bindings(node, content);
    // A declaration inside a function body is a local variable: recorded for receiver
    // inference, not emitted.
    if !out.locals.is_empty() {
        if let Some(scope) = out.locals.last_mut() {
            for (n, t) in &bindings {
                if let Some(t) = t {
                    scope.insert(n.clone(), t.clone());
                }
            }
        }
        for v in [node.child_by_field_name("value"), child_of_kind(node, "computed_property")]
            .into_iter()
            .flatten()
        {
            walk_body(v, content, out);
        }
        return;
    }
    let in_type = !out.types.is_empty();
    let kind = if in_type {
        "property"
    } else if is_let {
        "constant"
    } else {
        "variable"
    };
    let annotation = child_of_kind(node, "type_annotation").and_then(type_child).map(|t| node_text(t, content));
    let single = bindings.len() == 1;
    for (name, ty) in &bindings {
        let span = if single { node } else { node.child_by_field_name("name").unwrap_or(node) };
        let mut sym = new_symbol(kind, name, out.qualify(name), out.container().as_deref(), span, content, docs(node, content));
        sym.visibility = visibility_of(&mods)
            .or_else(|| if in_type { out.types.last().and_then(|t| t.member_visibility.clone()) } else { None })
            .or_else(|| Some("internal".into()));
        sym.is_exported = matches!(sym.visibility.as_deref(), Some("public" | "open"));
        sym.is_static = mods.iter().any(|m| m == "static" || m == "class") || (!in_type && is_let);
        sym.is_abstract = node.kind() == "protocol_property_declaration";
        sym.return_type = annotation.clone();
        sym.signature = header(node, content);
        if let Some(t) = ty {
            out.refs.push(decl_ref("type_of", &sym, t, node));
            if !in_type {
                out.globals.insert(name.clone(), t.clone());
            }
        }
        push_override(&mods, &sym, name, node, out);
        out.symbols.push(sym);
    }
    for v in [
        node.child_by_field_name("value"),
        node.child_by_field_name("computed_value"),
        child_of_kind(node, "computed_property"),
        child_of_kind(node, "willset_didset_block"),
    ]
    .into_iter()
    .flatten()
    {
        out.locals.push(HashMap::new());
        walk_body(v, content, out);
        out.locals.pop();
    }
}

// ---------------------------------------------------------------------------
// Bodies: calls and local declarations
// ---------------------------------------------------------------------------

fn walk_body(node: TsNode, content: &str, out: &mut SwOut) {
    match node.kind() {
        "class_declaration" | "protocol_declaration" => {
            // A type declared inside a function body is still a type of the enclosing scope.
            let saved = std::mem::take(&mut out.locals);
            visit(node, content, out);
            out.locals = saved;
            return;
        }
        "function_declaration" => {
            function_decl(node, content, out);
            return;
        }
        "property_declaration" => {
            property_decl(node, content, out);
            return;
        }
        "call_expression" => push_call(node, content, out),
        _ => {}
    }
    for ch in named_children(node) {
        walk_body(ch, content, out);
    }
}

/// Argument labels of a call: `_` for an unlabeled argument, `*` for a trailing closure.
fn call_labels(suffix: TsNode, content: &str) -> Vec<String> {
    let mut labels = Vec::new();
    for ch in named_children(suffix) {
        match ch.kind() {
            "value_arguments" => {
                for arg in named_children(ch) {
                    if arg.kind() != "value_argument" {
                        continue;
                    }
                    labels.push(
                        arg.child_by_field_name("name")
                            .map(|n| node_text(n, content).trim().to_string())
                            .unwrap_or_else(|| "_".to_string()),
                    );
                }
            }
            "lambda_literal" => labels.push("*".to_string()),
            _ => {}
        }
    }
    labels
}

/// Project type of a receiver expression: a typed local / parameter, `self.prop` or an
/// implicit-self property of the enclosing type.
fn receiver_type(recv: &str, out: &SwOut) -> Option<String> {
    let recv = recv.trim().trim_end_matches(['?', '!']);
    if let Some(prop) = recv.strip_prefix("self.") {
        let prop = prop.trim_end_matches(['?', '!']);
        return out.types.last().and_then(|t| t.props.get(prop).cloned());
    }
    if !recv.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    out.local_type(recv)
        .or_else(|| out.types.last().and_then(|t| t.props.get(recv).cloned()))
}

fn push_call(call: TsNode, content: &str, out: &mut SwOut) {
    let Some(callee) = call.named_child(0) else { return };
    let Some(suffix) = child_of_kind(call, "call_suffix") else { return };
    // `dict["key"]` / `items[0]` parse as calls with bracketed arguments: subscripts, not calls.
    if child_of_kind(suffix, "value_arguments").is_some_and(|a| node_text(a, content).starts_with('[')) {
        return;
    }
    let labels = call_labels(suffix, content);
    let selector = |name: &str| format!("{}({})", name, labels.iter().map(|l| format!("{}:", l)).collect::<String>());
    let line = call.start_position().row + 1;
    let col = call.start_position().column;
    let mk = |target: String, receiver: Option<String>, receiver_type: Option<String>| ExtractedCall {
        caller_name: String::new(),
        is_method: receiver.is_some(),
        target_name: target,
        receiver,
        qualifier: None,
        receiver_type,
        line,
        col,
    };
    match callee.kind() {
        // `foo(..)` (implicit self or a free function) and `T(..)` (initializer).
        "simple_identifier" => {
            let name = node_text(callee, content);
            // `defer { .. }` parses as a call with a trailing closure.
            if name.starts_with('$') || name == "defer" {
                return;
            }
            out.calls.push(mk(selector(&name), None, None));
        }
        "navigation_expression" => {
            let Some(name) = callee
                .child_by_field_name("suffix")
                .and_then(|s| s.child_by_field_name("suffix").or(Some(s)))
                .map(|s| node_text(s, content).trim_start_matches('.').to_string())
            else {
                return;
            };
            let Some(target) = callee.child_by_field_name("target") else {
                // `.member(..)`: an implicit member of a contextual type.
                return;
            };
            let recv_text = node_text(target, content);
            match target.kind() {
                "super_expression" => {
                    let sup = out.types.last().and_then(|t| t.superclass.clone());
                    out.calls.push(mk(selector(&name), Some("super".into()), sup));
                }
                "self_expression" => {
                    if name == "init" {
                        // `self.init(..)`: delegating initializer.
                        let mut c = mk(selector("init"), Some("self".into()), None);
                        c.is_method = true;
                        out.calls.push(c);
                    } else {
                        out.calls.push(mk(selector(&name), Some("self".into()), None));
                    }
                }
                _ if name == "init" => {
                    // `T.init(..)` constructs `T`.
                    if let Some(t) = swift_type_name(&recv_text) {
                        out.calls.push(mk(selector(&t), None, None));
                    }
                }
                _ => {
                    // `T(..).m()`: the receiver is a fresh `T`.
                    let rt = receiver_type(&recv_text, out).or_else(|| initializer_type(target, content));
                    let recv = recv_text.split_whitespace().collect::<String>();
                    out.calls.push(mk(selector(&name), Some(recv), rt));
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Overloads
// ---------------------------------------------------------------------------

/// Qualify every member of an overload set (same kind + qualified name) with its selector
/// (`Shape.move(to:by:)`) so each declaration keeps its own node; parameters and declaration
/// references of a renamed callable follow it.
fn disambiguate_overloads(out: &mut SwOut) {
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for (i, _) in &out.selectors {
        let s = &out.symbols[*i];
        *counts.entry((s.kind.clone(), s.qualified_name.clone())).or_default() += 1;
    }
    // (start, end, old qualified name, new qualified name)
    let mut renamed: Vec<(usize, usize, String, String)> = Vec::new();
    for (i, selector) in &out.selectors {
        let s = &mut out.symbols[*i];
        if counts.get(&(s.kind.clone(), s.qualified_name.clone())).copied().unwrap_or(0) < 2 {
            continue;
        }
        let old = s.qualified_name.clone();
        let prefix = old.rsplit_once('.').map(|(p, _)| format!("{}.", p)).unwrap_or_default();
        let new = format!("{}{}", prefix, selector);
        renamed.push((s.start_line, s.end_line, old, new.clone()));
        s.qualified_name = new;
    }
    if renamed.is_empty() {
        return;
    }
    let follow = |q: &str, line: usize| -> Option<String> {
        for (start, end, old, new) in &renamed {
            if line < *start || line > *end {
                continue;
            }
            if q == old {
                return Some(new.clone());
            }
            if let Some(rest) = q.strip_prefix(&format!("{}.", old)) {
                return Some(format!("{}.{}", new, rest));
            }
        }
        None
    };
    let renamed_set: std::collections::HashSet<String> = renamed.iter().map(|r| r.3.clone()).collect();
    for s in out.symbols.iter_mut() {
        if renamed_set.contains(&s.qualified_name) {
            continue;
        }
        if let Some(q) = follow(&s.qualified_name, s.start_line) {
            s.qualified_name = q;
        }
    }
    for r in out.refs.iter_mut() {
        if r.from.is_empty() || r.from_kind == "extension" {
            continue;
        }
        if let Some(q) = follow(&r.from, r.line) {
            r.from = q;
        }
    }
}
