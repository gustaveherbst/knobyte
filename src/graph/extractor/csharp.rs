//! C# extraction (tree-sitter-c-sharp).
//!
//! Emits namespaces, classes, structs, records (as `class`, or `struct` for `record struct`),
//! interfaces, enums (+ members), delegates (`type_alias`), methods, constructors, destructors,
//! operators, local functions, properties, indexers, events, fields, constants and parameters,
//! plus `using` imports, calls, instantiations, attributes (`decorates`) and inheritance.
//!
//! Grammar notes: `base_list` does not say which entry is the base class, so — as in the
//! conventional C# layout — the first entry of a class/struct/record is `extends` and the rest
//! `implements`; every entry of an interface is `extends`. Overloaded members share a qualified
//! name, so every member of an overload set is qualified with its parameter types
//! (`Ns.Type.Add(int, int)`) to keep one node per declaration.

use super::*;

const TYPE_DECLS: [&str; 4] = [
    "class_declaration",
    "struct_declaration",
    "record_declaration",
    "interface_declaration",
];
const METHOD_DECLS: [&str; 6] = [
    "method_declaration",
    "constructor_declaration",
    "destructor_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
    "local_function_statement",
];

#[derive(Default)]
struct CsOut {
    symbols: Vec<ExtractedSymbol>,
    imports: Vec<ExtractedImport>,
    refs: Vec<ExtractedRef>,
    calls: Vec<ExtractedCall>,
    /// Qualified-name prefix (namespaces and enclosing types), outermost first.
    scope: Vec<String>,
    /// Enclosing type simple names, innermost last (the resolver's `container` key).
    types: Vec<String>,
    /// Index of the symbol owning parameters / bodies being walked.
    file_lines: usize,
}

impl CsOut {
    fn qualify(&self, name: &str) -> String {
        if self.scope.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", self.scope.join("."), name)
        }
    }
}

fn modifiers(node: TsNode, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() == "modifier" {
            out.push(node_text(child, content).trim().to_string());
        }
    }
    out
}

fn visibility(mods: &[String]) -> Option<String> {
    let has = |m: &str| mods.iter().any(|x| x == m);
    if has("protected") && has("internal") {
        return Some("protected internal".into());
    }
    if has("private") && has("protected") {
        return Some("private protected".into());
    }
    ["public", "private", "protected", "internal"]
        .iter()
        .find(|v| has(v))
        .map(|v| v.to_string())
}

/// Declaration header up to (not including) the body: `public int Add(int a, int b)`.
fn header(node: TsNode, content: &str) -> Option<String> {
    let mut text = node_text(node, content);
    // Leading attribute lists (`[Serializable]`) are not part of the header.
    loop {
        let t = text.trim_start();
        if !t.starts_with('[') {
            break;
        }
        let mut depth = 0i32;
        let mut end = None;
        for (i, ch) in t.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => text = t[e..].to_string(),
            None => break,
        }
    }
    let cut = text.find(['{', ';']).unwrap_or(text.len());
    let cut = text[..cut].find("=>").unwrap_or(cut);
    let h = text[..cut].split_whitespace().collect::<Vec<_>>().join(" ");
    (!h.is_empty()).then_some(h)
}

/// The project type a C# type annotation names, unwrapping task/sequence wrappers.
fn cs_type_name(t: &str) -> Option<String> {
    let mut t = t.trim().trim_end_matches('?').to_string();
    for w in [
        "Task", "ValueTask", "IEnumerable", "IAsyncEnumerable", "IList", "ICollection",
        "IReadOnlyList", "IReadOnlyCollection", "Nullable", "Lazy",
    ] {
        if let Some(inner) = t
            .strip_prefix(w)
            .and_then(|r| r.strip_prefix('<'))
            .and_then(|r| r.strip_suffix('>'))
        {
            t = inner.to_string();
        }
    }
    primary_type_name(&t)
}

fn cs_docs(node: TsNode, content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut row = node.start_position().row;
    let mut doc = Vec::new();
    while row > 0 {
        row -= 1;
        let l = lines.get(row).map(|l| l.trim()).unwrap_or("");
        if l.starts_with("///") {
            doc.push(l.trim_start_matches('/').trim().to_string());
        } else if l.starts_with('[') && doc.is_empty() {
            continue;
        } else {
            break;
        }
    }
    if doc.is_empty() {
        return None;
    }
    doc.reverse();
    let text = doc
        .join(" ")
        .replace("<summary>", "")
        .replace("</summary>", "")
        .trim()
        .to_string();
    (!text.is_empty()).then_some(text)
}

fn method_name(node: TsNode, content: &str, out: &CsOut) -> Option<String> {
    match node.kind() {
        "operator_declaration" => {
            let op = field_text(node, "operator", content)?;
            Some(format!("operator {}", op.trim()))
        }
        "conversion_operator_declaration" => {
            let t = field_text(node, "type", content)?;
            let which = if node_text(node, content).contains("implicit") {
                "implicit"
            } else {
                "explicit"
            };
            Some(format!("{} operator {}", which, t.trim()))
        }
        "constructor_declaration" | "destructor_declaration" => {
            let name = field_text(node, "name", content)
                .or_else(|| out.types.last().cloned())?;
            Some(if node.kind() == "destructor_declaration" {
                format!("~{}", name.trim_start_matches('~'))
            } else {
                name
            })
        }
        _ => field_text(node, "name", content),
    }
}

pub(super) fn extract_csharp(content: &str) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = tree_sitter_c_sharp::LANGUAGE.into();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let mut out = CsOut {
        file_lines: content.lines().count().max(1),
        ..Default::default()
    };
    walk_children(root, content, &mut out);
    disambiguate_overloads(&mut out.symbols, &mut out.refs);
    Some(ExtractionResult {
        language: "csharp".to_string(),
        symbols: out.symbols,
        calls: out.calls,
        imports: out.imports,
        trait_impls: Vec::new(),
        refs: out.refs,
        parse_status: parse_status_of(root),
    })
}

fn walk_children(node: TsNode, content: &str, out: &mut CsOut) {
    let mut c = node.walk();
    let children: Vec<TsNode> = node.named_children(&mut c).collect();
    let mut i = 0;
    while i < children.len() {
        let child = children[i];
        if child.kind() == "file_scoped_namespace_declaration" {
            // `namespace Foo;` owns every later sibling.
            let name = field_text(child, "name", content).unwrap_or_default();
            let mut sym = new_symbol("namespace", &name, out.qualify(&name), None, child, content, None);
            sym.end_line = out.file_lines;
            sym.end_col = usize::MAX / 4;
            sym.is_exported = true;
            out.symbols.push(sym);
            out.scope.push(name);
            for sib in &children[i + 1..] {
                visit(*sib, content, out);
            }
            out.scope.pop();
            return;
        }
        visit(child, content, out);
        i += 1;
    }
}

fn visit(node: TsNode, content: &str, out: &mut CsOut) {
    let kind = node.kind();
    match kind {
        "namespace_declaration" => {
            let name = field_text(node, "name", content).unwrap_or_default();
            if name.is_empty() {
                return;
            }
            let mut sym = new_symbol("namespace", &name, out.qualify(&name), None, node, content, None);
            sym.is_exported = true;
            out.symbols.push(sym);
            out.scope.push(name);
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(body, content, out);
            }
            out.scope.pop();
        }
        k if TYPE_DECLS.contains(&k) => type_decl(node, content, out),
        "enum_declaration" => {
            let Some(name) = field_text(node, "name", content) else { return };
            let mods = modifiers(node, content);
            let mut sym = new_symbol(
                "enum",
                &name,
                out.qualify(&name),
                out.types.last().map(String::as_str),
                node,
                content,
                cs_docs(node, content),
            );
            sym.visibility = visibility(&mods);
            sym.is_exported = mods.iter().any(|m| m == "public");
            sym.signature = header(node, content);
            out.symbols.push(sym);
            let q = out.qualify(&name);
            if let Some(body) = node.child_by_field_name("body") {
                let mut c = body.walk();
                for m in body.named_children(&mut c) {
                    if m.kind() != "enum_member_declaration" {
                        continue;
                    }
                    if let Some(mn) = field_text(m, "name", content) {
                        let mut ms =
                            new_symbol("enum_member", &mn, format!("{}.{}", q, mn), Some(&name), m, content, None);
                        ms.is_exported = true;
                        out.symbols.push(ms);
                    }
                }
            }
        }
        "delegate_declaration" => {
            let Some(name) = field_text(node, "name", content) else { return };
            let mods = modifiers(node, content);
            let mut sym = new_symbol(
                "type_alias",
                &name,
                out.qualify(&name),
                out.types.last().map(String::as_str),
                node,
                content,
                cs_docs(node, content),
            );
            sym.visibility = visibility(&mods);
            sym.is_exported = mods.iter().any(|m| m == "public");
            sym.return_type = field_text(node, "type", content);
            sym.signature = header(node, content);
            out.symbols.push(sym);
        }
        k if METHOD_DECLS.contains(&k) => method_decl(node, content, out),
        "property_declaration" | "indexer_declaration" | "event_declaration" => property_decl(node, content, out),
        "field_declaration" | "event_field_declaration" => field_decl(node, content, out),
        "using_directive" => {
            let text = node_text(node, content);
            let is_static = text.contains("static ");
            let mut c = node.walk();
            let named: Vec<TsNode> = node.named_children(&mut c).collect();
            let target = match (node.child_by_field_name("name"), named.len()) {
                (Some(a), n) if n >= 2 => named
                    .iter()
                    .rev()
                    .find(|ch| ch.id() != a.id())
                    .map(|t| (Some(node_text(a, content)), node_text(*t, content))),
                _ => named.first().map(|n| (None, node_text(*n, content))),
            };
            if let Some((alias, module)) = target {
                let module = module.trim().to_string();
                if module.is_empty() {
                    return;
                }
                let last = module.rsplit('.').next().unwrap_or(&module).to_string();
                let (imported, local, is_module) = match alias {
                    Some(a) => (last, a.trim().to_string(), false),
                    None => {
                        let _ = is_static;
                        ("*".to_string(), String::new(), true)
                    }
                };
                out.imports.push(ExtractedImport {
                    imported_name: imported,
                    source_module: module,
                    local_name: local,
                    is_module,
                    is_type_only: false,
                    is_reexport: text.trim_start().starts_with("global "),
                    line: node.start_position().row + 1,
                });
            }
        }
        _ => walk_children(node, content, out),
    }
}

fn attributes(node: TsNode, content: &str, sym: &ExtractedSymbol, refs: &mut Vec<ExtractedRef>) {
    let mut c = node.walk();
    for list in node.named_children(&mut c) {
        if list.kind() != "attribute_list" {
            continue;
        }
        let mut c2 = list.walk();
        for attr in list.named_children(&mut c2) {
            if attr.kind() != "attribute" {
                continue;
            }
            if let Some(name) = field_text(attr, "name", content) {
                if let Some((n, q)) = split_type_ref(&name) {
                    // `[HttpGet]` names the `HttpGetAttribute` class.
                    refs.push(named_ref("decorates", sym, &n, q, attr));
                }
            }
        }
    }
}

fn type_decl(node: TsNode, content: &str, out: &mut CsOut) {
    let Some(name) = field_text(node, "name", content) else { return };
    let text = node_text(node, content);
    let kind = match node.kind() {
        "struct_declaration" => "struct",
        "interface_declaration" => "interface",
        "record_declaration" if text.split('{').next().unwrap_or("").contains("record struct") => "struct",
        _ => "class",
    };
    let mods = modifiers(node, content);
    let mut sym = new_symbol(
        kind,
        &name,
        out.qualify(&name),
        out.types.last().map(String::as_str),
        node,
        content,
        cs_docs(node, content),
    );
    sym.visibility = visibility(&mods);
    sym.is_exported = mods.iter().any(|m| m == "public");
    sym.is_abstract = mods.iter().any(|m| m == "abstract") || kind == "interface";
    sym.is_static = mods.iter().any(|m| m == "static");
    sym.signature = header(node, content);
    // Heritage.
    let mut c = node.walk();
    if let Some(bases) = node.named_children(&mut c).find(|ch| ch.kind() == "base_list") {
        let mut c2 = bases.walk();
        let entries: Vec<TsNode> = bases
            .named_children(&mut c2)
            .filter(|e| e.kind() != "argument_list")
            .collect();
        for (i, e) in entries.iter().enumerate() {
            let t = match e.kind() {
                "primary_constructor_base_type" => e
                    .named_child(0)
                    .map(|n| node_text(n, content))
                    .unwrap_or_default(),
                _ => node_text(*e, content),
            };
            let rel = if kind == "interface" || i == 0 && !is_interface_name(&t) {
                "extends"
            } else {
                "implements"
            };
            if let Some((n, q)) = split_type_ref(&t) {
                out.refs.push(named_ref(rel, &sym, &n, q, *e));
            }
        }
    }
    attributes(node, content, &sym, &mut out.refs);
    // Positional record parameters are properties.
    let record_params = if node.kind() == "record_declaration" {
        let mut c3 = node.walk();
        let found = node.named_children(&mut c3).find(|ch| ch.kind() == "parameter_list");
        found
    } else {
        None
    };
    let q = sym.qualified_name.clone();
    out.symbols.push(sym);
    if let Some(params) = record_params {
        let mut c4 = params.walk();
        for p in params.named_children(&mut c4) {
            if p.kind() != "parameter" {
                continue;
            }
            if let Some(pn) = field_text(p, "name", content) {
                let mut ps = new_symbol("property", &pn, format!("{}.{}", q, pn), Some(&name), p, content, None);
                ps.visibility = Some("public".into());
                ps.is_exported = true;
                ps.return_type = field_text(p, "type", content);
                ps.signature = Some(node_text(p, content));
                out.symbols.push(ps);
            }
        }
    }
    out.scope.push(name.clone());
    out.types.push(name);
    if let Some(body) = node.child_by_field_name("body") {
        walk_children(body, content, out);
    }
    out.types.pop();
    out.scope.pop();
}

/// .NET convention: interface names are `I` + PascalCase (`IDisposable`).
fn is_interface_name(t: &str) -> bool {
    let base = strip_generics(t);
    let name = base.rsplit('.').next().unwrap_or(&base);
    let mut ch = name.chars();
    ch.next() == Some('I') && ch.next().is_some_and(|c| c.is_ascii_uppercase())
}

fn method_decl(node: TsNode, content: &str, out: &mut CsOut) {
    let Some(name) = method_name(node, content, out) else { return };
    let local = node.kind() == "local_function_statement";
    let mods = modifiers(node, content);
    let container = if local { None } else { out.types.last().cloned() };
    let qualified = out.qualify(&name);
    let mut sym = new_symbol(
        if local || out.types.is_empty() { "function" } else { "method" },
        &name,
        qualified.clone(),
        container.as_deref(),
        node,
        content,
        cs_docs(node, content),
    );
    sym.visibility = visibility(&mods).or_else(|| (!local).then(|| "private".to_string()));
    sym.is_exported = mods.iter().any(|m| m == "public");
    sym.is_static = mods.iter().any(|m| m == "static");
    sym.is_abstract = mods.iter().any(|m| m == "abstract");
    sym.is_async = mods.iter().any(|m| m == "async");
    sym.return_type = field_text(node, "returns", content)
        .or_else(|| field_text(node, "type", content))
        .map(|t| t.trim().to_string());
    sym.signature = header(node, content);
    if let Some(rt) = sym.return_type.as_deref().and_then(cs_type_name) {
        out.refs.push(decl_ref("returns", &sym, &rt, node));
    }
    attributes(node, content, &sym, &mut out.refs);
    parameters(node, content, &sym, out);
    out.symbols.push(sym);
    // Constructor chaining: `: base(..)` / `: this(..)`.
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        if ch.kind() == "constructor_initializer" {
            walk_body(ch, content, out);
        }
    }
    if let Some(body) = node.child_by_field_name("body") {
        out.scope.push(name);
        walk_body(body, content, out);
        out.scope.pop();
    }
}

fn parameters(node: TsNode, content: &str, owner: &ExtractedSymbol, out: &mut CsOut) {
    let Some(params) = node
        .child_by_field_name("parameters")
        .or_else(|| {
            let mut c = node.walk();
            let found = node
                .named_children(&mut c)
                .find(|ch| ch.kind() == "bracketed_parameter_list" || ch.kind() == "parameter_list");
            found
        })
    else {
        return;
    };
    let mut c = params.walk();
    for p in params.named_children(&mut c) {
        if p.kind() != "parameter" {
            continue;
        }
        let Some(pn) = field_text(p, "name", content) else { continue };
        let mut ps = new_symbol(
            "parameter",
            &pn,
            format!("{}.{}", owner.qualified_name, pn),
            None,
            p,
            content,
            None,
        );
        ps.return_type = field_text(p, "type", content);
        ps.signature = Some(node_text(p, content).split_whitespace().collect::<Vec<_>>().join(" "));
        if let Some(t) = ps.return_type.as_deref().and_then(cs_type_name) {
            out.refs.push(decl_ref("type_of", &ps, &t, p));
        }
        out.symbols.push(ps);
    }
}

fn property_decl(node: TsNode, content: &str, out: &mut CsOut) {
    let name = if node.kind() == "indexer_declaration" {
        "this".to_string()
    } else {
        match field_text(node, "name", content) {
            Some(n) => n,
            None => return,
        }
    };
    let mods = modifiers(node, content);
    let mut sym = new_symbol(
        "property",
        &name,
        out.qualify(&name),
        out.types.last().map(String::as_str),
        node,
        content,
        cs_docs(node, content),
    );
    sym.visibility = visibility(&mods).or_else(|| Some("private".into()));
    sym.is_exported = mods.iter().any(|m| m == "public");
    sym.is_static = mods.iter().any(|m| m == "static");
    sym.is_abstract = mods.iter().any(|m| m == "abstract");
    sym.return_type = field_text(node, "type", content);
    sym.signature = header(node, content);
    if let Some(t) = sym.return_type.as_deref().and_then(cs_type_name) {
        out.refs.push(decl_ref("type_of", &sym, &t, node));
    }
    attributes(node, content, &sym, &mut out.refs);
    if node.kind() == "indexer_declaration" {
        parameters(node, content, &sym, out);
    }
    out.symbols.push(sym);
    for f in ["accessors", "value"] {
        if let Some(b) = node.child_by_field_name(f) {
            walk_body(b, content, out);
        }
    }
}

fn field_decl(node: TsNode, content: &str, out: &mut CsOut) {
    let mods = modifiers(node, content);
    let is_const = mods.iter().any(|m| m == "const");
    let mut c = node.walk();
    let Some(var) = node
        .named_children(&mut c)
        .find(|ch| ch.kind() == "variable_declaration")
    else {
        return;
    };
    let ty = field_text(var, "type", content);
    let mut c2 = var.walk();
    for d in var.named_children(&mut c2) {
        if d.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = field_text(d, "name", content).or_else(|| d.named_child(0).map(|n| node_text(n, content)))
        else {
            continue;
        };
        let mut sym = new_symbol(
            if is_const { "constant" } else { "field" },
            &name,
            out.qualify(&name),
            out.types.last().map(String::as_str),
            d,
            content,
            cs_docs(node, content),
        );
        sym.visibility = visibility(&mods).or_else(|| Some("private".into()));
        sym.is_exported = mods.iter().any(|m| m == "public");
        sym.is_static = is_const || mods.iter().any(|m| m == "static");
        sym.return_type = ty.clone();
        sym.signature = header(node, content);
        if let Some(t) = ty.as_deref().and_then(cs_type_name) {
            out.refs.push(decl_ref("type_of", &sym, &t, node));
        }
        attributes(node, content, &sym, &mut out.refs);
        out.symbols.push(sym);
        walk_body(d, content, out);
    }
}

/// Calls and instantiations inside a body; nested declarations are visited as declarations.
fn walk_body(node: TsNode, content: &str, out: &mut CsOut) {
    match node.kind() {
        k if METHOD_DECLS.contains(&k) || TYPE_DECLS.contains(&k) => {
            visit(node, content, out);
            return;
        }
        "invocation_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                push_call(f, node, content, out);
            }
        }
        "object_creation_expression" => {
            if let Some(t) = field_text(node, "type", content) {
                if let Some((n, q)) = split_type_ref(&t) {
                    out.refs.push(scoped_ref("instantiates", &n, q, node));
                }
            }
        }
        _ => {}
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        walk_body(ch, content, out);
    }
}

fn push_call(f: TsNode, call: TsNode, content: &str, out: &mut CsOut) {
    let line = call.start_position().row + 1;
    let col = call.start_position().column;
    let mk = |target: String, receiver: Option<String>| ExtractedCall {
        caller_name: String::new(),
        target_name: strip_generics(&target),
        receiver,
        qualifier: None,
        receiver_type: None,
        is_method: true,
        line,
        col,
    };
    match f.kind() {
        // Unqualified `Foo()` inside a type is `this.Foo()` (or a static member).
        "identifier" | "generic_name" => {
            let name = node_text(f, content);
            let receiver = if out.types.is_empty() { None } else { Some("this".to_string()) };
            let mut c = mk(name, receiver.clone());
            c.is_method = receiver.is_some();
            out.calls.push(c);
        }
        "member_access_expression" => {
            let Some(name) = field_text(f, "name", content) else { return };
            let recv = f
                .child_by_field_name("expression")
                .map(|e| node_text(e, content))
                .or_else(|| {
                    let mut c = f.walk();
                    let r = f
                        .children(&mut c)
                        .find(|ch| ch.kind() == "this" || ch.kind() == "base")
                        .map(|ch| node_text(ch, content));
                    r
                })
                .map(|r| if r == "base" { "this".to_string() } else { r });
            out.calls.push(mk(name, recv));
        }
        "member_binding_expression" => {
            if let Some(name) = field_text(f, "name", content) {
                out.calls.push(mk(name, Some("?".into())));
            }
        }
        _ => {}
    }
}

/// Qualify each member of an overload set (same kind + qualified name) with its parameter
/// types so each declaration keeps its own node.
fn disambiguate_overloads(symbols: &mut [ExtractedSymbol], refs: &mut [ExtractedRef]) {
    use std::collections::HashMap;
    // Indexers (`this[int]`, `this[string]`) are an overload set of properties named `this`.
    let overloadable = |s: &ExtractedSymbol| {
        matches!(s.kind.as_str(), "method" | "function") || (s.kind == "property" && s.name == "this")
    };
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for s in symbols.iter() {
        if overloadable(s) {
            *counts.entry((s.kind.clone(), s.qualified_name.clone())).or_default() += 1;
        }
    }
    let mut renamed: HashMap<String, String> = HashMap::new();
    for s in symbols.iter_mut() {
        if !overloadable(s) || counts.get(&(s.kind.clone(), s.qualified_name.clone())).copied().unwrap_or(0) < 2 {
            continue;
        }
        let (open_ch, close_ch) = if s.kind == "property" { ('[', ']') } else { ('(', ')') };
        let params = s
            .signature
            .as_deref()
            .and_then(|sig| {
                // An indexer's parameter list follows `this`.
                let from = if s.kind == "property" { sig.find("this").unwrap_or(0) } else { 0 };
                let open = from + sig[from..].find(open_ch)?;
                let close = sig.rfind(close_ch)?;
                (close > open).then(|| sig[open + 1..close].to_string())
            })
            .unwrap_or_default();
        let types: Vec<String> = split_params(&params)
            .iter()
            .map(|p| {
                let p = p.trim();
                let p = p.split('=').next().unwrap_or(p).trim();
                let mut words: Vec<&str> = p.split_whitespace().collect();
                words.retain(|w| !matches!(*w, "ref" | "out" | "in" | "params" | "this"));
                if words.len() > 1 {
                    words.pop();
                }
                words.join(" ")
            })
            .filter(|t| !t.is_empty())
            .collect();
        let old = s.qualified_name.clone();
        let new = format!("{}({})", old, types.join(", "));
        renamed.insert(format!("{}@{}", old, s.start_line), new.clone());
        s.qualified_name = new;
    }
    if renamed.is_empty() {
        return;
    }
    // Parameters of a renamed overload follow their owner.
    let owners: Vec<(usize, usize, String, String)> = symbols
        .iter()
        .filter(|s| renamed.values().any(|v| v == &s.qualified_name))
        .map(|s| {
            let base = s.qualified_name.split('(').next().unwrap_or("").to_string();
            (s.start_line, s.end_line, base, s.qualified_name.clone())
        })
        .collect();
    for s in symbols.iter_mut() {
        if s.kind != "parameter" {
            continue;
        }
        for (start, end, base, q) in &owners {
            if s.start_line >= *start && s.start_line <= *end && s.qualified_name == format!("{}.{}", base, s.name) {
                s.qualified_name = format!("{}.{}", q, s.name);
                break;
            }
        }
    }
    // Declaration references made from a renamed overload (or its parameters) follow it.
    for r in refs.iter_mut() {
        if r.from.is_empty() {
            continue;
        }
        for (start, end, base, q) in &owners {
            if r.line < *start || r.line > *end {
                continue;
            }
            if r.from == *base && r.from_kind != "parameter" {
                r.from = q.clone();
                break;
            }
            if r.from_kind == "parameter" {
                let prefix = format!("{}.", base);
                if let Some(pname) = r.from.strip_prefix(&prefix) {
                    if !pname.contains('.') {
                        r.from = format!("{}.{}", q, pname);
                        break;
                    }
                }
            }
        }
    }
}

fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '<' | '(' | '[' => {
                depth += 1;
                cur.push(ch)
            }
            '>' | ')' | ']' => {
                depth -= 1;
                cur.push(ch)
            }
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}
