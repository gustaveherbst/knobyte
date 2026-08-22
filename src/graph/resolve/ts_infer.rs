//! Source-only TypeScript / JavaScript receiver-type inference.
//!
//! Without a type checker, a member call `recv.m()` only binds when the receiver's type is
//! known. The extractor describes each member call's receiver as a small, context-free type
//! expression ([`TExpr`]): a base (`T` from an annotation, a cast or `new T()`; `this` / `super`
//! of the enclosing class; the return type of a free function; a module-level variable or class)
//! followed by bounded property / method / `await` steps. It also records per-file type facts
//! (`ts_type` references): class and interface field types (annotations, initializers, constructor
//! parameter properties, constructor `this.x = ..` assignments, getters), annotated return types,
//! module-level variable types, `extends` / `implements` clauses and type aliases.
//!
//! At build time [`SymbolIndex::resolve_ts_inferred`] evaluates the expression across files:
//! type names bind only through lexical scope or explicit imports (barrels, tsconfig paths and
//! default exports included), members are looked up on the type and its `extends` chain, and
//! every step must name exactly one declaration. Anything uncertain (a union, an unresolved or
//! ambiguous name, a type parameter, an untyped value) yields no answer and the call falls back to
//! the conservative resolver. Edges carry provenance `ts-inference` and confidence
//! [`CONFIDENCE`], below the type-checker mode's.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{Binding, CallResolution, SymbolIndex};
use crate::graph::models::{ExtractedCall, ExtractedRef};

/// Provenance and resolution method of inferred call edges.
pub(crate) const PROVENANCE: &str = "ts-inference";
/// Confidence of inferred call edges (the type-checker mode's edges are 1.0).
pub(crate) const CONFIDENCE: f64 = 0.9;
/// Reference kind of extracted type facts (consumed by the build, never edges).
pub(crate) const FACT_KIND: &str = "ts_type";
/// Prefix of an encoded receiver expression in `ExtractedCall::receiver_type`.
const RECEIVER_PREFIX: &str = "ts:";
/// Evaluation depth bound (alias, variable and member hops, inheritance walks).
const MAX_DEPTH: usize = 16;

/// Fact roles (`ExtractedRef::from_kind` of a `ts_type` reference).
pub(crate) mod role {
    /// Declared field / property / getter type (`owner.name`).
    pub const FIELD: &str = "field";
    /// Field type from a constructor `this.name = ..` assignment (used when nothing is declared).
    pub const FIELD_INIT: &str = "field_init";
    /// Annotated return type of a method (`owner.name`) or free function (`name`).
    pub const RETURNS: &str = "returns";
    /// Module-level variable type.
    pub const VAR: &str = "var";
    pub const EXTENDS: &str = "extends";
    pub const IMPLEMENTS: &str = "implements";
    /// `type name = T`.
    pub const ALIAS: &str = "alias";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Base {
    /// A type written in the source (normalized annotation text, `Promise<T>` kept).
    Type(String),
    /// `this` inside class `C`.
    This(String),
    /// `super` inside class `C`.
    Super(String),
    /// The value returned by calling the free function `f` visible in the file.
    Fn(String),
    /// A name not bound locally: a module-level variable, a class, or a namespace import.
    Var(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Step {
    Field(String),
    Call(String),
    Await,
}

/// A receiver (or fact) type expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TExpr {
    pub base: Base,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
}

impl TExpr {
    pub fn new(base: Base) -> Self {
        TExpr { base, steps: Vec::new() }
    }

    pub fn with(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    /// Call links in the chain (a `Fn` base counts as one).
    pub fn call_links(&self) -> usize {
        self.steps.iter().filter(|s| matches!(s, Step::Call(_))).count()
            + matches!(self.base, Base::Fn(_)) as usize
    }

    pub fn encode(&self) -> String {
        format!("{}{}", RECEIVER_PREFIX, serde_json::to_string(self).unwrap_or_default())
    }

    pub fn decode(text: &str) -> Option<Self> {
        serde_json::from_str(text.strip_prefix(RECEIVER_PREFIX)?).ok()
    }
}

/// True for a `receiver_type` produced by this module (not a plain type name).
pub(crate) fn is_encoded(text: &str) -> bool {
    text.starts_with(RECEIVER_PREFIX)
}

/// A written type that names exactly one (possibly dotted) type: `Svc`, `models.Svc`,
/// `Repo<User>` (`Repo`), `Svc | undefined` (`Svc`), `Promise<Svc>` (`Svc`, awaited).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedType {
    pub path: String,
    pub promise: bool,
}

/// Split at top-level occurrences of `sep` (outside `<..>`, `(..)`, `[..]`, `{..}`).
fn split_top(text: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out
}

pub(crate) fn parse_type(text: &str) -> Option<NamedType> {
    let normalized = normalize_type_text(text);
    let text = normalized.as_str();
    let members: Vec<&str> = split_top(text, '|')
        .into_iter()
        .map(str::trim)
        .filter(|m| !m.is_empty() && !matches!(*m, "null" | "undefined"))
        .collect();
    let [single] = members.as_slice() else { return None };
    let t = single.trim();
    if t.is_empty() || t.contains(['&', '{', '(', '[', '"', '\'', '`', ' ', '=']) {
        return None;
    }
    let (head, args) = match t.find('<') {
        Some(pos) if t.ends_with('>') => (&t[..pos], Some(&t[pos + 1..t.len() - 1])),
        Some(_) => return None,
        None => (t, None),
    };
    if head == "Promise" || head == "PromiseLike" {
        let inner = parse_type(args?)?;
        if inner.promise {
            return None;
        }
        return Some(NamedType {
            path: inner.path,
            promise: true,
        });
    }
    let valid = !head.is_empty()
        && head
            .split('.')
            .all(|s| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$'))
        && !head.starts_with(|c: char| c.is_ascii_digit());
    valid.then(|| NamedType {
        path: head.to_string(),
        promise: false,
    })
}

/// Normalized type text (whitespace collapsed around punctuation) for a fact or base.
pub(crate) fn normalize_type_text(text: &str) -> String {
    let joined = text.trim().trim_start_matches(':').split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(joined.len());
    let chars: Vec<char> = joined.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' {
            let prev = if i > 0 { chars[i - 1] } else { ' ' };
            let next = chars.get(i + 1).copied().unwrap_or(' ');
            let punct = |ch: char| matches!(ch, '<' | '>' | ',' | '|' | '.' | '[' | ']' | '(' | ')');
            if punct(prev) || punct(next) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// An `extends` / `implements` clause: (role, written type).
type Clause = (String, Option<TExpr>);

#[derive(Debug, Clone)]
struct Fact {
    role: String,
    expr: Option<TExpr>,
}

/// Type facts of the corpus plus every TS/JS file's import bindings.
#[derive(Default)]
pub(crate) struct TsTypes {
    /// (file, owner, name) -> facts in source order (owner "" for module-level declarations).
    facts: HashMap<(String, String, String), Vec<Fact>>,
    /// (file, owner) -> `extends` / `implements` clauses.
    heritage: HashMap<(String, String), Vec<Clause>>,
    bindings: HashMap<String, Vec<Binding>>,
}

/// A value an expression evaluates to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Val {
    /// An instance of a class or interface node.
    Inst(usize),
    /// A class (constructor) value: static members.
    Class(usize),
    /// A namespace import of a project file.
    Module(String),
    /// A promise of a value (`await` unwraps it).
    Promise(Box<Val>),
}

/// Member lookup outcome along an inheritance chain.
enum Lookup<T> {
    Found(T),
    Missing,
    /// Ambiguous or unknowable: stop, do not try further bases.
    Abort,
}

const TYPE_KINDS: [&str; 3] = ["class", "interface", "type_alias"];

fn is_ts_like(file: &str) -> bool {
    [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .any(|e| file.ends_with(e))
}

impl SymbolIndex {
    /// Record one extracted `ts_type` fact of `file`.
    pub fn add_ts_fact(&mut self, file: &str, r: &ExtractedRef) {
        let expr = r.qualifier.as_deref().and_then(TExpr::decode);
        let types = &mut self.ts_types;
        match r.from_kind.as_str() {
            role::EXTENDS | role::IMPLEMENTS => types
                .heritage
                .entry((file.to_string(), r.from.clone()))
                .or_default()
                .push((r.from_kind.clone(), expr)),
            _ => types
                .facts
                .entry((file.to_string(), r.from.clone(), r.target_name.clone()))
                .or_default()
                .push(Fact {
                    role: r.from_kind.clone(),
                    expr,
                }),
        }
    }

    /// Make every TS/JS file's import bindings available to cross-file inference.
    pub fn set_ts_bindings(&mut self, bindings: &HashMap<String, Vec<Binding>>) {
        self.ts_types.bindings = bindings
            .iter()
            .filter(|(f, _)| is_ts_like(f))
            .map(|(f, b)| (f.clone(), b.clone()))
            .collect();
    }

    /// The method a TS/JS member call binds to by receiver-type inference, if certain.
    pub(super) fn resolve_ts_inferred(&self, file: &str, call: &ExtractedCall) -> Option<CallResolution> {
        if !call.is_method {
            return None;
        }
        let expr = TExpr::decode(call.receiver_type.as_deref()?)?;
        let recv = self.ts_eval(file, &expr, 0)?;
        let target = match recv {
            Val::Inst(t) | Val::Class(t) => match self.ts_find_method(t, &call.target_name, 0, &mut HashSet::new()) {
                Lookup::Found(m) => m,
                _ => return None,
            },
            // Namespace imports and promises are left to the regular resolver.
            Val::Module(_) | Val::Promise(_) => return None,
        };
        Some(Self::edge(target, "calls", CONFIDENCE, PROVENANCE))
    }

    fn ts_bindings(&self, file: &str) -> &[Binding] {
        self.ts_types.bindings.get(file).map(Vec::as_slice).unwrap_or(&[])
    }

    /// A name visible in `file` of one of `kinds`, bound by lexical scope or an explicit import
    /// only (never by repository-wide uniqueness or an import of an unrelated file).
    fn ts_resolve_name(&self, file: &str, name: &str, qualifier: Option<&str>, kinds: &[&str]) -> Option<usize> {
        let bindings = self.ts_bindings(file);
        if qualifier.is_none() {
            if let Some(b) = bindings.iter().find(|b| b.local == name) {
                if b.imported == "default" {
                    let t = b.resolved_file.as_deref().and_then(|f| self.default_export_of(f))?;
                    return kinds.contains(&self.metas[t].kind.as_str()).then_some(t);
                }
            }
        }
        match self.resolve_named(file, name, qualifier, kinds, bindings, None) {
            CallResolution::Edge {
                target,
                method: "lexical-scope" | "explicit-import" | "namespace_member",
                ..
            } => Some(target),
            _ => None,
        }
    }

    /// The class or interface a written type names (type aliases followed).
    fn ts_resolve_type(&self, file: &str, path: &str, depth: usize) -> Option<usize> {
        if depth > MAX_DEPTH {
            return None;
        }
        let (qualifier, name) = match path.rsplit_once('.') {
            Some((q, n)) => (Some(q), n),
            None => (None, path),
        };
        let t = self.ts_resolve_name(file, name, qualifier, &TYPE_KINDS)?;
        let m = &self.metas[t];
        match m.kind.as_str() {
            "class" | "interface" => Some(t),
            "type_alias" => {
                let facts = self.ts_types.facts.get(&(m.file_path.clone(), String::new(), m.name.clone()))?;
                let alias = Self::single_expr(facts.iter().filter(|f| f.role == role::ALIAS))?;
                match &alias.base {
                    Base::Type(text) if alias.steps.is_empty() => {
                        let nt = parse_type(text).filter(|nt| !nt.promise)?;
                        self.ts_resolve_type(&m.file_path, &nt.path, depth + 1)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The one expression of a fact set (all facts present and equal), else `None`.
    fn single_expr<'a>(mut facts: impl Iterator<Item = &'a Fact>) -> Option<&'a TExpr> {
        let first = facts.next()?.expr.as_ref()?;
        for f in facts {
            if f.expr.as_ref() != Some(first) {
                return None;
            }
        }
        Some(first)
    }

    fn ts_type_value(&self, file: &str, text: &str, recv: Option<&Val>, depth: usize) -> Option<Val> {
        if text == "this" {
            return recv.cloned();
        }
        let nt = parse_type(text)?;
        let inst = Val::Inst(self.ts_resolve_type(file, &nt.path, depth + 1)?);
        Some(if nt.promise { Val::Promise(Box::new(inst)) } else { inst })
    }

    fn ts_class_in_file(&self, file: &str, name: &str) -> Option<usize> {
        let cands: Vec<usize> = self
            .by_file
            .get(file)?
            .iter()
            .copied()
            .filter(|&i| self.metas[i].kind == "class" && self.metas[i].name == name)
            .collect();
        (cands.len() == 1).then(|| cands[0])
    }

    /// The single base class of a class.
    fn ts_superclass(&self, class: usize, depth: usize) -> Option<usize> {
        let m = &self.metas[class];
        let clauses = self.ts_types.heritage.get(&(m.file_path.clone(), m.name.clone()))?;
        let mut ext = clauses.iter().filter(|(r, _)| r == role::EXTENDS);
        let (_, expr) = ext.next()?;
        if ext.next().is_some() {
            return None;
        }
        match &expr.as_ref()?.base {
            Base::Type(text) => {
                let nt = parse_type(text).filter(|nt| !nt.promise)?;
                self.ts_resolve_type(&m.file_path, &nt.path, depth + 1)
            }
            _ => None,
        }
    }

    /// Bases searched for members: a class's `extends`; an interface's `extends` list.
    fn ts_bases(&self, t: usize, depth: usize) -> Option<Vec<usize>> {
        let m = &self.metas[t];
        if m.kind == "class" {
            let has_extends = self
                .ts_types
                .heritage
                .get(&(m.file_path.clone(), m.name.clone()))
                .is_some_and(|c| c.iter().any(|(r, _)| r == role::EXTENDS));
            if !has_extends {
                return Some(Vec::new());
            }
            // A base class that cannot be resolved (external, mixin) hides inherited members.
            return self.ts_superclass(t, depth).map(|b| vec![b]);
        }
        let mut out = Vec::new();
        for (r, expr) in self
            .ts_types
            .heritage
            .get(&(m.file_path.clone(), m.name.clone()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if r != role::EXTENDS {
                continue;
            }
            let Some(TExpr { base: Base::Type(text), .. }) = expr else { return None };
            let nt = parse_type(text).filter(|nt| !nt.promise)?;
            out.push(self.ts_resolve_type(&m.file_path, &nt.path, depth + 1)?);
        }
        Some(out)
    }

    /// Declarations named `name` lexically inside type node `t`.
    fn ts_own_members(&self, t: usize, name: &str) -> Vec<usize> {
        let tm = &self.metas[t];
        self.members_named(&tm.name, name)
            .into_iter()
            .filter(|&i| {
                let m = &self.metas[i];
                m.file_path == tm.file_path && tm.contains(m.start_line, m.start_col)
            })
            .collect()
    }

    /// Search `t` then its bases with `own`; several distinct answers from parallel bases abort.
    fn ts_walk<T: Clone + PartialEq>(
        &self,
        t: usize,
        depth: usize,
        visited: &mut HashSet<usize>,
        own: &dyn Fn(usize) -> Lookup<T>,
    ) -> Lookup<T> {
        if depth > MAX_DEPTH || !visited.insert(t) {
            return Lookup::Abort;
        }
        match own(t) {
            Lookup::Missing => {}
            other => return other,
        }
        let Some(bases) = self.ts_bases(t, depth) else { return Lookup::Abort };
        let mut found: Option<T> = None;
        for b in bases {
            match self.ts_walk(b, depth + 1, visited, own) {
                Lookup::Found(x) => {
                    if found.as_ref().is_some_and(|f| *f != x) {
                        return Lookup::Abort;
                    }
                    found = Some(x);
                }
                Lookup::Missing => {}
                Lookup::Abort => return Lookup::Abort,
            }
        }
        found.map(Lookup::Found).unwrap_or(Lookup::Missing)
    }

    /// The method (or function-typed property) `name` of `t` or its bases.
    fn ts_find_method(&self, t: usize, name: &str, depth: usize, visited: &mut HashSet<usize>) -> Lookup<usize> {
        let own = |t: usize| -> Lookup<usize> {
            let members = self.ts_own_members(t, name);
            let callables: Vec<usize> = members.iter().copied().filter(|&i| self.metas[i].is_callable()).collect();
            match callables.len() {
                1 => return Lookup::Found(callables[0]),
                0 => {}
                _ => return Lookup::Abort,
            }
            let props: Vec<usize> = members
                .iter()
                .copied()
                .filter(|&i| matches!(self.metas[i].kind.as_str(), "property" | "field"))
                .collect();
            match props.len() {
                0 => Lookup::Missing,
                1 if !self.ts_field_aliases_function(t, name) => Lookup::Found(props[0]),
                _ => Lookup::Abort,
            }
        };
        self.ts_walk(t, depth, visited, &own)
    }

    /// True when field `name` of `t` is declared `typeof f`: a call through it calls `f`.
    fn ts_field_aliases_function(&self, t: usize, name: &str) -> bool {
        let m = &self.metas[t];
        self.ts_types
            .facts
            .get(&(m.file_path.clone(), m.name.clone(), name.to_string()))
            .is_some_and(|facts| {
                facts.iter().any(|f| {
                    matches!(&f.expr, Some(TExpr { base: Base::Type(text), .. }) if text.starts_with("typeof "))
                })
            })
    }

    /// The declared type expression of field `name` of `t` or its bases: (owner file, expr).
    fn ts_find_field(&self, t: usize, name: &str, depth: usize) -> Lookup<(String, TExpr)> {
        let own = |t: usize| -> Lookup<(String, TExpr)> {
            let m = &self.metas[t];
            if let Some(facts) = self.ts_types.facts.get(&(m.file_path.clone(), m.name.clone(), name.to_string())) {
                let declared: Vec<&Fact> = facts.iter().filter(|f| f.role == role::FIELD).collect();
                let chosen = if declared.is_empty() {
                    Self::single_expr(facts.iter().filter(|f| f.role == role::FIELD_INIT))
                } else {
                    Self::single_expr(declared.into_iter())
                };
                if facts.iter().any(|f| f.role == role::FIELD || f.role == role::FIELD_INIT) {
                    return match chosen {
                        Some(e) => Lookup::Found((m.file_path.clone(), e.clone())),
                        None => Lookup::Abort,
                    };
                }
            }
            // A member of that name without a usable field type (a method, an untyped field).
            if self.ts_own_members(t, name).is_empty() {
                Lookup::Missing
            } else {
                Lookup::Abort
            }
        };
        self.ts_walk(t, depth, &mut HashSet::new(), &own)
    }

    /// The annotated return type of callable `f` as a value.
    fn ts_return_value(&self, f: usize, recv: Option<&Val>, depth: usize) -> Option<Val> {
        let m = &self.metas[f];
        let owner = m.container.clone().unwrap_or_default();
        let facts = self.ts_types.facts.get(&(m.file_path.clone(), owner, m.name.clone()))?;
        let expr = Self::single_expr(facts.iter().filter(|x| x.role == role::RETURNS))?;
        match &expr.base {
            Base::Type(text) if expr.steps.is_empty() => self.ts_type_value(&m.file_path, text, recv, depth + 1),
            _ => None,
        }
    }

    /// A module-level variable's value.
    fn ts_var_value(&self, v: usize, depth: usize) -> Option<Val> {
        let m = &self.metas[v];
        let facts = self.ts_types.facts.get(&(m.file_path.clone(), String::new(), m.name.clone()))?;
        let expr = Self::single_expr(facts.iter().filter(|x| x.role == role::VAR))?.clone();
        self.ts_eval(&m.file_path.clone(), &expr, depth + 1)
    }

    /// A top-level declaration a module exports under `name` (barrels followed).
    fn ts_module_member(&self, file: &str, name: &str) -> Option<usize> {
        self.top_level_in_file(file, name)
            .or_else(|| self.follow_reexport(file, name, 0))
            .filter(|&t| !self.metas[t].is_synthetic())
    }

    fn ts_value_of_decl(&self, t: usize, depth: usize) -> Option<Val> {
        match self.metas[t].kind.as_str() {
            "class" => Some(Val::Class(t)),
            "constant" | "variable" => self.ts_var_value(t, depth),
            _ => None,
        }
    }

    fn ts_base(&self, file: &str, base: &Base, depth: usize) -> Option<Val> {
        match base {
            Base::Type(text) => self.ts_type_value(file, text, None, depth),
            Base::This(c) => self.ts_class_in_file(file, c).map(Val::Inst),
            Base::Super(c) => {
                let class = self.ts_class_in_file(file, c)?;
                self.ts_superclass(class, depth).map(Val::Inst)
            }
            Base::Fn(name) => {
                let f = self.ts_resolve_name(file, name, None, &["function"])?;
                self.ts_return_value(f, None, depth)
            }
            Base::Var(name) => {
                if let Some(b) = self.ts_bindings(file).iter().find(|b| b.local == *name) {
                    let binds_file = b.target.is_some_and(|t| self.metas[t].kind == "file");
                    if b.is_module || binds_file {
                        return b.resolved_file.clone().map(Val::Module);
                    }
                }
                let t = self.ts_resolve_name(file, name, None, &["class", "constant", "variable"])?;
                self.ts_value_of_decl(t, depth)
            }
        }
    }

    fn ts_eval(&self, file: &str, expr: &TExpr, depth: usize) -> Option<Val> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut val = self.ts_base(file, &expr.base, depth + 1)?;
        for step in &expr.steps {
            val = match step {
                Step::Await => match val {
                    Val::Promise(inner) => *inner,
                    other => other,
                },
                Step::Field(name) => match &val {
                    Val::Inst(t) | Val::Class(t) => match self.ts_find_field(*t, name, depth + 1) {
                        Lookup::Found((owner_file, e)) => self.ts_eval(&owner_file, &e, depth + 1)?,
                        _ => return None,
                    },
                    Val::Module(f) => {
                        let t = self.ts_module_member(f, name)?;
                        self.ts_value_of_decl(t, depth + 1)?
                    }
                    Val::Promise(_) => return None,
                },
                Step::Call(name) => match &val {
                    Val::Inst(t) | Val::Class(t) => match self.ts_find_method(*t, name, depth + 1, &mut HashSet::new()) {
                        Lookup::Found(m) if self.metas[m].is_callable() => self.ts_return_value(m, Some(&val), depth + 1)?,
                        _ => return None,
                    },
                    Val::Module(f) => {
                        let t = self.ts_module_member(f, name).filter(|&t| self.metas[t].kind == "function")?;
                        self.ts_return_value(t, None, depth + 1)?
                    }
                    Val::Promise(_) => return None,
                },
            };
        }
        Some(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_types() {
        let p = |t: &str| parse_type(t).map(|n| (n.path, n.promise));
        assert_eq!(p(": Svc"), Some(("Svc".into(), false)));
        assert_eq!(p("models.Svc"), Some(("models.Svc".into(), false)));
        assert_eq!(p("Repo<User, Map<string, X>>"), Some(("Repo".into(), false)));
        assert_eq!(p("Svc | undefined"), Some(("Svc".into(), false)));
        assert_eq!(p("null | Svc"), Some(("Svc".into(), false)));
        assert_eq!(p("Promise<Svc>"), Some(("Svc".into(), true)));
        assert_eq!(p("Promise<Svc | null>"), Some(("Svc".into(), true)));
        assert_eq!(p("A | B"), None);
        assert_eq!(p("Svc[]"), None);
        assert_eq!(p("{ a: Svc }"), None);
        assert_eq!(p("() => Svc"), None);
        assert_eq!(p("typeof svc"), None);
        assert_eq!(p("Promise<Promise<X>>"), None);
        assert_eq!(normalize_type_text(": Map < string , X >"), "Map<string,X>");
    }

    #[test]
    fn expressions_round_trip() {
        let e = TExpr::new(Base::This("C".into()))
            .with(Step::Field("repo".into()))
            .with(Step::Call("find".into()))
            .with(Step::Await);
        let s = e.encode();
        assert!(is_encoded(&s));
        assert_eq!(TExpr::decode(&s), Some(e.clone()));
        assert_eq!(e.call_links(), 1);
    }
}
