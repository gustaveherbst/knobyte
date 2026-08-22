//! Swift name resolution on top of the generic resolver.
//!
//! Every file of a Swift module sees every other file's declarations without an import, so a
//! name that is neither local nor imported is looked up in the caller's module: a SwiftPM
//! target (`Sources/<Target>/..`, `Tests/<Target>/..`) or, outside SwiftPM, the top-level
//! directory (an Xcode target folder). `import M` / `@testable import M` of an in-repo target
//! adds that module's public (`@testable`: all) declarations.
//!
//! Calls carry their argument labels in the target name (`move(to:by:)`, `load()`); the base
//! name resolves as usual and the labels pick among overloads, whose qualified names end with
//! their selector.

use super::*;

/// (module root path, module name) of a Swift file.
pub(crate) fn swift_module_of(file: &str) -> (String, String) {
    let segs: Vec<&str> = file.split('/').collect();
    if segs.len() >= 3 {
        if let Some(i) = (0..segs.len() - 2)
            .rev()
            .find(|&i| matches!(segs[i], "Sources" | "Tests"))
        {
            return (segs[..=i + 1].join("/"), segs[i + 1].to_string());
        }
    }
    if segs.len() > 1 {
        return (segs[0].to_string(), segs[0].to_string());
    }
    (String::new(), String::new())
}

/// Base name and argument labels of a call target: `move(to:by:)` -> (`move`, [`to`, `by`]).
fn split_selector(target: &str) -> (&str, Option<Vec<&str>>) {
    match target.find('(') {
        Some(i) if target.ends_with(')') => {
            let inner = &target[i + 1..target.len() - 1];
            let labels = inner.split(':').filter(|s| !s.is_empty()).collect();
            (&target[..i], Some(labels))
        }
        _ => (target, None),
    }
}

/// Argument labels of a declaration whose qualified name ends with its selector.
fn decl_labels(qualified_name: &str) -> Option<Vec<&str>> {
    let open = qualified_name.rfind('(')?;
    if !qualified_name.ends_with(')') {
        return None;
    }
    Some(
        qualified_name[open + 1..qualified_name.len() - 1]
            .split(':')
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// `*` (a trailing closure, whose label the call does not show) matches any label.
fn labels_match(call: &[&str], decl: &[&str]) -> bool {
    call.len() == decl.len() && call.iter().zip(decl).all(|(c, d)| *c == "*" || c == d)
}

impl SymbolIndex {
    /// Keep the overloads whose selector matches the call's labels, when that narrows the set.
    fn swift_by_labels(&self, cands: Vec<usize>, labels: Option<&[&str]>) -> Vec<usize> {
        let Some(labels) = labels else { return cands };
        if cands.len() < 2 {
            return cands;
        }
        let matching: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| decl_labels(&self.metas[i].qualified_name).is_some_and(|d| labels_match(labels, &d)))
            .collect();
        if matching.is_empty() {
            cands
        } else {
            matching
        }
    }

    /// Whether a declaration in `cand` is visible from `file` as a module-scope name.
    fn swift_visible(&self, file: &str, cand: usize, bindings: &[Binding]) -> bool {
        let m = &self.metas[cand];
        if !m.file_path.ends_with(".swift") {
            return false;
        }
        let (root, _) = swift_module_of(file);
        let (cand_root, cand_module) = swift_module_of(&m.file_path);
        if cand_root == root {
            return true;
        }
        bindings.iter().any(|b| {
            b.is_module && b.local == cand_module && (m.is_exported || b.imported == "@testable")
        })
    }

    /// Module-scope declarations named `name` visible from `file`, accepted by `ok`.
    fn swift_module_scope(
        &self,
        file: &str,
        name: &str,
        bindings: &[Binding],
        ok: impl Fn(&NodeMeta) -> bool,
    ) -> Vec<usize> {
        let cands: Vec<usize> = self
            .by_name
            .get(name)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|&i| ok(&self.metas[i]) && self.swift_visible(file, i, bindings))
                    .collect()
            })
            .unwrap_or_default();
        self.prefer_file(cands, file)
    }

    pub(super) fn swift_resolve_call(
        &self,
        file: &str,
        call: &ExtractedCall,
        caller: Option<usize>,
        bindings: &[Binding],
    ) -> CallResolution {
        let (base, labels) = split_selector(&call.target_name);
        let labels = labels.as_deref();
        let recv = call.receiver.as_deref().map(str::trim);
        let implicit = recv.is_none() && !call.is_method;
        let caller_container = caller.and_then(|c| self.metas[c].container.clone());
        let members = |container: &str, method: &'static str| -> Option<CallResolution> {
            let cands = self.swift_by_labels(self.callable_members(container, base), labels);
            self.one_or_ambiguous(cands, file, 1.0, method)
        };

        // 1. Members of the enclosing type (its extensions share the container): `m()`,
        // `self.m()`, `Self.m()`.
        if implicit || recv.is_some_and(|r| r == "self" || r == "Self") {
            if let Some(res) = caller_container.as_deref().and_then(|c| members(c, "same_container")) {
                return res;
            }
        }
        // 2. A receiver whose type the extractor inferred (`let x = T()`, `let x: T`, a typed
        // property, `super`).
        if let Some(t) = call.receiver_type.as_deref() {
            if let Some(res) = members(t, "type_annotation") {
                return res;
            }
        }
        // 3. Static member of a project type: `T.m()`.
        if let Some(r) = recv.filter(|r| r.chars().next().is_some_and(char::is_uppercase)) {
            if r.chars().all(|c| c.is_alphanumeric() || c == '_') {
                if let Some(res) = members(r, "qualifier_type") {
                    return res;
                }
                // `Enum.someCase(..)`: constructing a case with associated values.
                let cases: Vec<usize> = self
                    .members_named(r, base)
                    .into_iter()
                    .filter(|&i| self.metas[i].kind == "enum_member")
                    .collect();
                if let [case] = self.prefer_file(cases, file).as_slice() {
                    return Self::edge(*case, "references", 1.0, "enum_case");
                }
            }
        }
        // 4. Unqualified free function or initializer `T(..)` declared anywhere in the module
        // (or a module it imports).
        if implicit {
            let cands = self.swift_module_scope(file, base, bindings, |m| {
                (m.is_callable() && m.container.is_none()) || m.is_type()
            });
            let cands = self.swift_by_labels(cands, labels);
            match cands.len() {
                0 => {}
                1 => return Self::edge(cands[0], "calls", 1.0, "swift-module"),
                _ => return CallResolution::Ambiguous(cands),
            }
        }
        let mut plain = call.clone();
        plain.target_name = base.to_string();
        match self.resolve_call_generic(file, &plain, caller, bindings) {
            CallResolution::Ambiguous(c) => {
                let c = self.swift_by_labels(c, labels);
                if c.len() == 1 {
                    Self::edge(c[0], "possible_call", 0.5, "global_unique_method")
                } else {
                    CallResolution::Ambiguous(c)
                }
            }
            other => other,
        }
    }

    pub(super) fn swift_resolve_named(
        &self,
        file: &str,
        name: &str,
        qualifier: Option<&str>,
        kinds: &[&str],
        bindings: &[Binding],
        from: Option<usize>,
    ) -> CallResolution {
        // `overrides` references name a callable by its selector (`init(name:)`).
        let (name, labels) = split_selector(name);
        let labels = labels.as_deref();
        let res = match self.resolve_named_generic(file, name, qualifier, kinds, bindings, from) {
            CallResolution::Ambiguous(c) => match self.swift_by_labels(c, labels).as_slice() {
                [one] => Self::edge(*one, "", 1.0, "qualifier_type"),
                many => CallResolution::Ambiguous(many.to_vec()),
            },
            other => other,
        };
        if matches!(res, CallResolution::Edge { .. }) {
            return res;
        }
        let kind_ok = |m: &NodeMeta| !m.is_synthetic() && (kinds.is_empty() || kinds.contains(&m.kind.as_str()));
        let cands = match qualifier.map(str::trim).filter(|q| !q.is_empty()) {
            // `Outer.Inner`: a member type of a module-visible type.
            Some(q) => {
                let owner = q.rsplit('.').next().unwrap_or(q);
                self.members_named(owner, name)
                    .into_iter()
                    .filter(|&i| kind_ok(&self.metas[i]) && self.swift_visible(file, i, bindings))
                    .collect()
            }
            None => {
                let all = self.swift_module_scope(file, name, bindings, |m| {
                    kind_ok(m) && !from.is_some_and(|f| std::ptr::eq(&self.metas[f], m))
                });
                // Top-level declarations shadow nested ones of the same name.
                let top: Vec<usize> = all.iter().copied().filter(|&i| self.metas[i].container.is_none()).collect();
                if top.is_empty() {
                    all
                } else {
                    top
                }
            }
        };
        let cands = self.swift_by_labels(cands, labels);
        match cands.len() {
            1 => Self::edge(cands[0], "", 0.9, "swift-module"),
            0 => res,
            _ => CallResolution::Ambiguous(cands),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_roots() {
        assert_eq!(
            swift_module_of("Sources/Core/Models/User.swift"),
            ("Sources/Core".to_string(), "Core".to_string())
        );
        assert_eq!(
            swift_module_of("pkg/Tests/CoreTests/UserTests.swift"),
            ("pkg/Tests/CoreTests".to_string(), "CoreTests".to_string())
        );
        assert_eq!(swift_module_of("App/View.swift"), ("App".to_string(), "App".to_string()));
        assert_eq!(swift_module_of("main.swift"), (String::new(), String::new()));
    }

    #[test]
    fn selectors() {
        assert_eq!(split_selector("move(to:by:)"), ("move", Some(vec!["to", "by"])));
        assert_eq!(split_selector("load()"), ("load", Some(vec![])));
        assert_eq!(split_selector("load"), ("load", None));
        assert_eq!(decl_labels("Shape.move(to:by:)"), Some(vec!["to", "by"]));
        assert!(labels_match(&["_", "*"], &["_", "completion"]));
        assert!(!labels_match(&["to"], &["to", "by"]));
    }
}
