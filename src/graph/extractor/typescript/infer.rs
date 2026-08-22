//! Extraction side of source-only TS/JS receiver-type inference (see
//! `graph::resolve::ts_infer`): per-call receiver expressions resolved against the file's lexical
//! scopes, and per-file type facts (`ts_type` references) the build evaluates across files.

use super::*;
use crate::graph::resolve::ts_infer::{normalize_type_text, parse_type, role, Base, Step, TExpr, FACT_KIND};

/// Chained call links followed in one receiver expression (`a.b().c().d().e()`).
const MAX_CALL_LINKS: usize = 4;
/// Steps (property, call, await) in one receiver expression.
const MAX_STEPS: usize = 8;
/// Local variable indirections followed (`const a = ..; const b = a.x(); b.y()`).
const MAX_LOCAL_DEPTH: usize = 6;

/// The encoded receiver type expression of a member call whose receiver is `obj`.
pub(super) fn receiver_expr(obj: TsNode, content: &str) -> Option<String> {
    expr_of(obj, content, 0).map(|e| e.encode())
}

fn bounded(e: TExpr) -> Option<TExpr> {
    (e.steps.len() <= MAX_STEPS && e.call_links() <= MAX_CALL_LINKS).then_some(e)
}

/// The type expression of an expression node, if it can be described without a checker.
fn expr_of(node: TsNode, content: &str, depth: usize) -> Option<TExpr> {
    if depth > MAX_LOCAL_DEPTH {
        return None;
    }
    let e = match node.kind() {
        "this" => TExpr::new(Base::This(enclosing_class(node, content)?)),
        "super" => TExpr::new(Base::Super(enclosing_class(node, content)?)),
        "identifier" => {
            let name = node_text(node, content);
            match lookup_local(node, &name, content, depth)? {
                Local::Typed(e) => e,
                Local::Outer => TExpr::new(Base::Var(name)),
            }
        }
        "parenthesized_expression" | "non_null_expression" | "satisfies_expression" => {
            expr_of(node.named_child(0)?, content, depth)?
        }
        "as_expression" => type_expr(node.named_child(1)?, node, content)?,
        "type_assertion" => {
            let args = node.named_child(0).filter(|a| a.kind() == "type_arguments")?;
            type_expr(args.named_child(0)?, node, content)?
        }
        "new_expression" => {
            let ctor = node.child_by_field_name("constructor")?;
            if !matches!(ctor.kind(), "identifier" | "member_expression" | "nested_identifier") {
                return None;
            }
            let text = node_text(ctor, content);
            if ctor.kind() == "identifier" {
                // A locally bound constructor (a parameter, a local class) is not the type name.
                if let Local::Typed(_) = lookup_local(ctor, &text, content, depth)? {
                    return None;
                }
            }
            type_expr_text(&text, node, content)?
        }
        "await_expression" => expr_of(node.named_child(0)?, content, depth)?.with(Step::Await),
        "member_expression" => {
            let obj = node.child_by_field_name("object")?;
            let prop = node.child_by_field_name("property")?;
            if !matches!(prop.kind(), "property_identifier" | "private_property_identifier") {
                return None;
            }
            expr_of(obj, content, depth)?.with(Step::Field(node_text(prop, content)))
        }
        "call_expression" => {
            let func = node.child_by_field_name("function")?;
            match func.kind() {
                "identifier" => {
                    let name = node_text(func, content);
                    match lookup_local(func, &name, content, depth)? {
                        Local::Outer => TExpr::new(Base::Fn(name)),
                        Local::Typed(_) => return None,
                    }
                }
                "member_expression" => {
                    let obj = func.child_by_field_name("object")?;
                    let prop = func.child_by_field_name("property")?;
                    if !matches!(prop.kind(), "property_identifier" | "private_property_identifier") {
                        return None;
                    }
                    expr_of(obj, content, depth)?.with(Step::Call(node_text(prop, content)))
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    bounded(e)
}

/// A written type as a `Type` base, unless it names a type parameter in scope at `at`.
fn type_expr(ty: TsNode, at: TsNode, content: &str) -> Option<TExpr> {
    type_expr_text(&node_text(ty, content), at, content)
}

fn type_expr_text(text: &str, at: TsNode, content: &str) -> Option<TExpr> {
    let text = normalize_type_text(text);
    let nt = parse_type(&text)?;
    let head = nt.path.split('.').next().unwrap_or(&nt.path);
    if let Some(param) = type_param_in_scope(at, head, content) {
        // `T extends Base`: members of a type parameter are its constraint's members.
        if nt.path != head || nt.promise {
            return None;
        }
        let constraint = param.child_by_field_name("constraint")?;
        let text = normalize_type_text(&node_text(constraint, content));
        let text = text.strip_prefix("extends").map(str::trim)?.to_string();
        let inner = parse_type(&text)?;
        let inner_head = inner.path.split('.').next().unwrap_or(&inner.path);
        if type_param_in_scope(param, inner_head, content).is_some() {
            return None;
        }
        return Some(TExpr::new(Base::Type(text)));
    }
    Some(TExpr::new(Base::Type(text)))
}

/// The type parameter `name` declared by `at` or one of its ancestors (innermost first).
fn type_param_in_scope<'t>(at: TsNode<'t>, name: &str, content: &str) -> Option<TsNode<'t>> {
    let mut cur = Some(at);
    while let Some(n) = cur {
        if let Some(tp) = n.child_by_field_name("type_parameters") {
            let mut c = tp.walk();
            let found = tp
                .named_children(&mut c)
                .find(|p| p.child_by_field_name("name").is_some_and(|nm| node_text(nm, content) == name));
            if found.is_some() {
                return found;
            }
        }
        cur = n.parent();
    }
    None
}

/// The class whose instance `this` denotes at `node` (arrow functions keep the outer `this`).
fn enclosing_class(node: TsNode, content: &str) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        match n.kind() {
            "function_declaration" | "function_expression" | "function" | "generator_function"
            | "generator_function_declaration" | "program" => return None,
            "method_definition" if n.parent().is_none_or(|p| p.kind() != "class_body") => return None,
            "class_body" => {
                let class = n.parent()?;
                return match class.kind() {
                    "class_declaration" | "abstract_class_declaration" => field_text(class, "name", content),
                    _ => None,
                };
            }
            _ => {}
        }
        cur = n.parent();
    }
    None
}

/// What a name means at a use site.
enum Local {
    /// Bound locally, with a describable type.
    Typed(TExpr),
    /// Not bound by any enclosing local scope: a module-level declaration or import.
    Outer,
}

/// The local binding of `name` visible at `at`: `None` when it is bound locally but its type
/// cannot be described (an untyped parameter, a loop variable, ...).
fn lookup_local(at: TsNode, name: &str, content: &str, depth: usize) -> Option<Local> {
    let mut cur = at.parent();
    let mut prev = at;
    while let Some(n) = cur {
        match n.kind() {
            "program" => {
                // Module scope: a top-level declaration or an import, resolved at build time.
                return Some(Local::Outer);
            }
            "statement_block" | "class_static_block" => {
                // A namespace body is module scope too.
                if n.parent().is_some_and(|p| matches!(p.kind(), "internal_module" | "module")) {
                    return Some(Local::Outer);
                }
                if let Some(found) = scan_block(n, name, content, depth) {
                    return found;
                }
            }
            "switch_body" => {
                let mut c = n.walk();
                for case in n.named_children(&mut c) {
                    if let Some(found) = scan_block(case, name, content, depth) {
                        return found;
                    }
                }
            }
            "for_statement" => {
                if let Some(init) = n.child_by_field_name("initializer") {
                    if let Some(found) = scan_declaration(init, name, content, depth) {
                        return found;
                    }
                }
            }
            "for_in_statement" => {
                if n.child_by_field_name("left").is_some_and(|l| pattern_binds(l, name, content)) {
                    return None;
                }
            }
            "catch_clause" => {
                if n.child_by_field_name("parameter").is_some_and(|p| pattern_binds(p, name, content)) {
                    return None;
                }
            }
            "function_declaration" | "function_expression" | "function" | "generator_function"
            | "generator_function_declaration" | "arrow_function" | "method_definition" => {
                // The own name of a function expression.
                if matches!(n.kind(), "function_expression" | "function" | "generator_function")
                    && field_text(n, "name", content).as_deref() == Some(name)
                {
                    return None;
                }
                if let Some(found) = scan_params(n, name, content) {
                    return found;
                }
                // `var` declared in a nested block of this function is hoisted here.
                if let Some(body) = n.child_by_field_name("body") {
                    if body.id() != prev.id() && hoisted_var(body, name, content) {
                        return None;
                    }
                    if body.id() == prev.id() && hoisted_var_nested(body, name, content) {
                        return None;
                    }
                }
            }
            "class" if field_text(n, "name", content).as_deref() == Some(name) => return None,
            _ => {}
        }
        prev = n;
        cur = n.parent();
    }
    Some(Local::Outer)
}

/// `var name` anywhere in a function body (not inside nested functions).
fn hoisted_var(body: TsNode, name: &str, content: &str) -> bool {
    if body.kind() == "variable_declaration" {
        let mut c = body.walk();
        return body.named_children(&mut c).any(|d| {
            d.kind() == "variable_declarator" && d.child_by_field_name("name").is_some_and(|p| pattern_binds(p, name, content))
        });
    }
    if is_function_like(body) {
        return false;
    }
    let mut c = body.walk();
    let found = body.named_children(&mut c).any(|ch| hoisted_var(ch, name, content));
    found
}

/// `var name` in a nested block of `body` (its direct statements were already scanned).
fn hoisted_var_nested(body: TsNode, name: &str, content: &str) -> bool {
    let mut c = body.walk();
    let found = body
        .named_children(&mut c)
        .filter(|ch| ch.kind() != "variable_declaration")
        .any(|ch| hoisted_var(ch, name, content));
    found
}

fn is_function_like(n: TsNode) -> bool {
    matches!(
        n.kind(),
        "function_declaration" | "function_expression" | "function" | "generator_function"
            | "generator_function_declaration" | "arrow_function" | "method_definition" | "class_declaration"
            | "class" | "abstract_class_declaration"
    )
}

/// Bindings a block's direct statements introduce for `name`: `Some(None)` bound but untyped.
fn scan_block(block: TsNode, name: &str, content: &str, depth: usize) -> Option<Option<Local>> {
    let mut c = block.walk();
    for stmt in block.named_children(&mut c) {
        let stmt = if stmt.kind() == "export_statement" {
            match stmt.child_by_field_name("declaration") {
                Some(d) => d,
                None => continue,
            }
        } else {
            stmt
        };
        match stmt.kind() {
            "lexical_declaration" | "variable_declaration" => {
                if let Some(found) = scan_declaration(stmt, name, content, depth) {
                    return Some(found);
                }
            }
            "function_declaration" | "generator_function_declaration" | "class_declaration"
            | "abstract_class_declaration" | "enum_declaration" | "internal_module"
                if field_text(stmt, "name", content).as_deref() == Some(name) =>
            {
                // A nested declaration: resolved by name in the file at build time.
                return Some(Some(Local::Outer));
            }
            _ => {}
        }
    }
    None
}

/// The binding a `const/let/var` declaration introduces for `name`.
fn scan_declaration(decl: TsNode, name: &str, content: &str, depth: usize) -> Option<Option<Local>> {
    if !matches!(decl.kind(), "lexical_declaration" | "variable_declaration") {
        return None;
    }
    let mut c = decl.walk();
    for d in decl.named_children(&mut c) {
        if d.kind() != "variable_declarator" {
            continue;
        }
        let Some(pattern) = d.child_by_field_name("name") else { continue };
        if !pattern_binds(pattern, name, content) {
            continue;
        }
        let source = || -> Option<TExpr> {
            if let Some(ty) = d.child_by_field_name("type") {
                return type_expr(ty, d, content);
            }
            let value = d.child_by_field_name("value")?;
            if is_function_value(value) {
                return None;
            }
            expr_of(value, content, depth + 1)
        };
        if pattern.kind() == "identifier" {
            return Some(source().map(Local::Typed));
        }
        // Destructuring: `const { a, b: { c } } = expr`.
        let path = pattern_path(pattern, name, content)?;
        let typed = path.and_then(|path| {
            let mut e = source()?;
            for f in path {
                e = e.with(Step::Field(f));
            }
            bounded(e)
        });
        return Some(typed.map(Local::Typed));
    }
    None
}

/// The binding a function's parameters introduce for `name`.
fn scan_params(func: TsNode, name: &str, content: &str) -> Option<Option<Local>> {
    if let Some(p) = func.child_by_field_name("parameter") {
        // `x => ..`
        return pattern_binds(p, name, content).then_some(None);
    }
    let params = func.child_by_field_name("parameters")?;
    let mut c = params.walk();
    for p in params.named_children(&mut c) {
        let (pattern, ty) = match p.kind() {
            "required_parameter" | "optional_parameter" => (p.child_by_field_name("pattern"), p.child_by_field_name("type")),
            _ => (Some(p), None),
        };
        let Some(pattern) = pattern else { continue };
        if !pattern_binds(pattern, name, content) {
            continue;
        }
        let Some(ty) = ty else { return Some(None) };
        if pattern.kind() == "identifier" {
            return Some(type_expr(ty, p, content).map(Local::Typed));
        }
        // `({ repo }: Deps) => ..`
        let typed = pattern_path(pattern, name, content)?.and_then(|path| {
            let mut e = type_expr(ty, p, content)?;
            for f in path {
                e = e.with(Step::Field(f));
            }
            bounded(e)
        });
        return Some(typed.map(Local::Typed));
    }
    None
}

/// True when a binding pattern binds `name`.
fn pattern_binds(pattern: TsNode, name: &str, content: &str) -> bool {
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => node_text(pattern, content) == name,
        "assignment_pattern" | "object_assignment_pattern" => pattern
            .child_by_field_name("left")
            .is_some_and(|l| pattern_binds(l, name, content)),
        "pair_pattern" => pattern
            .child_by_field_name("value")
            .is_some_and(|v| pattern_binds(v, name, content)),
        "required_parameter" | "optional_parameter" => pattern
            .child_by_field_name("pattern")
            .is_some_and(|v| pattern_binds(v, name, content)),
        "object_pattern" | "array_pattern" | "rest_pattern" => {
            let mut c = pattern.walk();
            let found = pattern.named_children(&mut c).any(|ch| pattern_binds(ch, name, content));
            found
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut c = pattern.walk();
            let found = pattern.named_children(&mut c).any(|d| {
                d.kind() == "variable_declarator"
                    && d.child_by_field_name("name").is_some_and(|n| pattern_binds(n, name, content))
            });
            found
        }
        _ => false,
    }
}

/// Property path to `name` inside an object pattern: `Some(Some(path))` for plain property
/// destructuring, `Some(None)` when bound some other way (array element, rest), `None` when not
/// bound here.
fn pattern_path(pattern: TsNode, name: &str, content: &str) -> Option<Option<Vec<String>>> {
    match pattern.kind() {
        "identifier" => (node_text(pattern, content) == name).then(|| Some(Vec::new())),
        "object_pattern" => {
            let mut c = pattern.walk();
            for prop in pattern.named_children(&mut c) {
                match prop.kind() {
                    "shorthand_property_identifier_pattern" if node_text(prop, content) == name => {
                        return Some(Some(vec![name.to_string()]));
                    }
                    "object_assignment_pattern" => {
                        let left = prop.child_by_field_name("left")?;
                        if left.kind() == "shorthand_property_identifier_pattern" && node_text(left, content) == name {
                            return Some(Some(vec![name.to_string()]));
                        }
                    }
                    "pair_pattern" => {
                        let key = prop.child_by_field_name("key")?;
                        let value = prop.child_by_field_name("value")?;
                        let value = if value.kind() == "assignment_pattern" {
                            value.child_by_field_name("left")?
                        } else {
                            value
                        };
                        if let Some(inner) = pattern_path(value, name, content) {
                            if key.kind() != "property_identifier" {
                                return Some(None);
                            }
                            return Some(inner.map(|mut p| {
                                p.insert(0, node_text(key, content));
                                p
                            }));
                        }
                    }
                    _ => {
                        if pattern_binds(prop, name, content) {
                            return Some(None);
                        }
                    }
                }
            }
            None
        }
        _ => pattern_binds(pattern, name, content).then_some(None),
    }
}

// ---------------------------------------------------------------------------
// Type facts
// ---------------------------------------------------------------------------

fn fact(role: &str, owner: &str, name: &str, expr: Option<TExpr>, at: TsNode) -> ExtractedRef {
    ExtractedRef {
        kind: FACT_KIND.to_string(),
        from: owner.to_string(),
        from_kind: role.to_string(),
        target_name: name.to_string(),
        // An undescribable declared type is still recorded: it hides weaker evidence.
        qualifier: Some(expr.map(|e| e.encode()).unwrap_or_default()),
        line: at.start_position().row + 1,
        col: at.start_position().column,
    }
}

/// Type facts of a file (see `graph::resolve::ts_infer`).
pub(super) fn type_facts(root: TsNode, content: &str) -> Vec<ExtractedRef> {
    let mut out = Vec::new();
    visit(root, content, &mut out);
    out
}

fn visit(node: TsNode, content: &str, out: &mut Vec<ExtractedRef>) {
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = field_text(node, "name", content) {
                class_facts(node, &name, content, out);
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(node, "name", content) {
                interface_facts(node, &name, content, out);
            }
        }
        "type_alias_declaration" => {
            if let (Some(name), Some(value)) = (field_text(node, "name", content), node.child_by_field_name("value")) {
                if node.child_by_field_name("type_parameters").is_none() {
                    out.push(fact(role::ALIAS, "", &name, type_expr(value, node, content), node));
                }
            }
        }
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = field_text(node, "name", content) {
                returns_fact("", &name, node, content, out);
            }
        }
        "lexical_declaration" | "variable_declaration" if super::ts_at_module_scope(node) => {
            let mut c = node.walk();
            for d in node.named_children(&mut c) {
                if d.kind() != "variable_declarator" {
                    continue;
                }
                let Some(name_node) = d.child_by_field_name("name").filter(|n| n.kind() == "identifier") else {
                    continue;
                };
                let name = node_text(name_node, content);
                let value = d.child_by_field_name("value");
                if let Some(v) = value.filter(|v| is_function_value(*v)) {
                    returns_fact("", &name, v, content, out);
                    continue;
                }
                let expr = match (d.child_by_field_name("type"), value) {
                    (Some(ty), _) => type_expr(ty, d, content),
                    (None, Some(v)) => expr_of(v, content, 0),
                    (None, None) => None,
                };
                out.push(fact(role::VAR, "", &name, expr, d));
            }
        }
        _ => {}
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        visit(child, content, out);
    }
}

/// A field's declared type. `typeof f` is kept verbatim (it never names a class): calling such a
/// property calls `f`, which the build must not mistake for the property itself.
fn field_type(ty: TsNode, at: TsNode, content: &str) -> Option<TExpr> {
    let text = normalize_type_text(&node_text(ty, content));
    if text.starts_with("typeof ") {
        return Some(TExpr::new(Base::Type(text)));
    }
    type_expr(ty, at, content)
}

/// A `returns` fact for an annotated function / method (`decl` holds the `return_type`).
fn returns_fact(owner: &str, name: &str, decl: TsNode, content: &str, out: &mut Vec<ExtractedRef>) {
    if let Some(rt) = decl.child_by_field_name("return_type") {
        let text = normalize_type_text(&node_text(rt, content));
        let expr = if text == "this" {
            Some(TExpr::new(Base::Type(text)))
        } else {
            type_expr_text(&text, decl, content)
        };
        out.push(fact(role::RETURNS, owner, name, expr, decl));
    }
}

fn heritage_facts(node: TsNode, owner: &str, content: &str, out: &mut Vec<ExtractedRef>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_heritage" => {
                let mut hc = child.walk();
                for part in child.named_children(&mut hc) {
                    let role = match part.kind() {
                        "extends_clause" => role::EXTENDS,
                        "implements_clause" => role::IMPLEMENTS,
                        // JavaScript: `class A extends B`.
                        _ => {
                            out.push(fact(role::EXTENDS, owner, "", type_expr(part, node, content), part));
                            continue;
                        }
                    };
                    if role == role::EXTENDS {
                        // `extends Base<T>`: the value expression (type arguments dropped).
                        let value = part.child_by_field_name("value").or_else(|| part.named_child(0));
                        if let Some(v) = value {
                            out.push(fact(role, owner, "", type_expr(v, node, content), v));
                        }
                        continue;
                    }
                    let mut pc = part.walk();
                    for v in part.named_children(&mut pc) {
                        out.push(fact(role, owner, "", type_expr(v, node, content), v));
                    }
                }
            }
            "extends_type_clause" => {
                let mut tc = child.walk();
                for t in child.named_children(&mut tc) {
                    out.push(fact(role::EXTENDS, owner, "", type_expr(t, node, content), t));
                }
            }
            _ => {}
        }
    }
}

fn class_facts(class: TsNode, owner: &str, content: &str, out: &mut Vec<ExtractedRef>) {
    heritage_facts(class, owner, content, out);
    let Some(body) = class.child_by_field_name("body") else { return };
    let mut c = body.walk();
    for member in body.named_children(&mut c) {
        match member.kind() {
            "public_field_definition" | "field_definition" => {
                let Some(name_node) = member.child_by_field_name("name").or_else(|| member.child_by_field_name("property"))
                else {
                    continue;
                };
                let name = node_text(name_node, content);
                let value = member.child_by_field_name("value");
                if let Some(v) = value.filter(|v| is_function_value(*v)) {
                    returns_fact(owner, &name, v, content, out);
                    continue;
                }
                let expr = match (member.child_by_field_name("type"), value) {
                    (Some(ty), _) => field_type(ty, member, content),
                    (None, Some(v)) => expr_of(v, content, 0),
                    // `private repo;`: typed by constructor assignments.
                    (None, None) => continue,
                };
                out.push(fact(role::FIELD, owner, &name, expr, member));
            }
            "method_definition" => {
                let Some(name) = field_text(member, "name", content) else { continue };
                if node_has_child_kind(member, "set") {
                    continue;
                }
                if node_has_child_kind(member, "get") {
                    let expr = member
                        .child_by_field_name("return_type")
                        .and_then(|rt| type_expr(rt, member, content));
                    out.push(fact(role::FIELD, owner, &name, expr, member));
                    continue;
                }
                if name == "constructor" {
                    constructor_facts(member, owner, content, out);
                    continue;
                }
                returns_fact(owner, &name, member, content, out);
            }
            "abstract_method_signature" => {
                if let Some(name) = field_text(member, "name", content) {
                    returns_fact(owner, &name, member, content, out);
                }
            }
            _ => {}
        }
    }
}

/// Constructor parameter properties (`constructor(private repo: Repo)`) and `this.x = ..`
/// assignments directly in the constructor body.
fn constructor_facts(ctor: TsNode, owner: &str, content: &str, out: &mut Vec<ExtractedRef>) {
    if let Some(params) = ctor.child_by_field_name("parameters") {
        let mut c = params.walk();
        for p in params.named_children(&mut c) {
            if !matches!(p.kind(), "required_parameter" | "optional_parameter") {
                continue;
            }
            let is_property = {
                let mut pc = p.walk();
                let found = p
                    .children(&mut pc)
                    .any(|ch| matches!(ch.kind(), "accessibility_modifier" | "readonly" | "override_modifier"));
                found
            };
            let Some(pattern) = p.child_by_field_name("pattern").filter(|n| n.kind() == "identifier") else {
                continue;
            };
            if is_property {
                let expr = p.child_by_field_name("type").and_then(|ty| field_type(ty, p, content));
                out.push(fact(role::FIELD, owner, &node_text(pattern, content), expr, p));
            }
        }
    }
    let Some(body) = ctor.child_by_field_name("body") else { return };
    let mut c = body.walk();
    for stmt in body.named_children(&mut c) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let Some(assign) = stmt.named_child(0).filter(|a| a.kind() == "assignment_expression") else { continue };
        let (Some(left), Some(right)) = (assign.child_by_field_name("left"), assign.child_by_field_name("right")) else {
            continue;
        };
        if left.kind() != "member_expression"
            || left.child_by_field_name("object").is_none_or(|o| o.kind() != "this")
        {
            continue;
        }
        let Some(prop) = left.child_by_field_name("property") else { continue };
        let expr = if is_function_value(right) { None } else { expr_of(right, content, 0) };
        out.push(fact(role::FIELD_INIT, owner, &node_text(prop, content), expr, assign));
    }
}

fn interface_facts(node: TsNode, owner: &str, content: &str, out: &mut Vec<ExtractedRef>) {
    heritage_facts(node, owner, content, out);
    let Some(body) = node.child_by_field_name("body") else { return };
    let mut c = body.walk();
    for member in body.named_children(&mut c) {
        let Some(name) = member.child_by_field_name("name").map(|n| strip_quotes(&node_text(n, content))) else {
            continue;
        };
        match member.kind() {
            "property_signature" => {
                let expr = member.child_by_field_name("type").and_then(|ty| field_type(ty, member, content));
                out.push(fact(role::FIELD, owner, &name, expr, member));
            }
            "method_signature" => returns_fact(owner, &name, member, content, out),
            _ => {}
        }
    }
}
