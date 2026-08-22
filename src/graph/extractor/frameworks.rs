//! Framework route resolvers: HTTP routes declared through Express, FastAPI, Flask, NestJS and
//! Next.js (App Router `route.*` files and Pages Router `pages/api/**`) become `route` nodes
//! named `METHOD /path`, each referencing its handler in the same file (edge
//! `route -references-> handler`, provenance `framework`).
//!
//! Every resolver is syntactic and file-local: a route is emitted only when the file itself
//! proves the framework (an import of it, or a receiver created from it), and the handler is
//! bound only to a uniquely named declaration of the same file.
//!
//! Express router mounts (`app.use('/api', router)`, `app.use('/api', [mw, router])`, mounts
//! onto imported routers) and `router.route('/x').get(..).post(..)` chains are recorded as link
//! facts (the route's receiver, each mount, each exported router) and composed across files by
//! the build (`graph::links`), so a router's routes carry the full mounted path at any nesting
//! depth. Python
//! `app.include_router(r, prefix=..)` is not followed.

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

use super::*;

/// A route discovered by a resolver.
struct Route {
    framework: &'static str,
    method: String,
    path: String,
    /// Handler name as declared in the file (`list`, or `UsersController.findOne`).
    handler: String,
    line: usize,
    /// Receiver the route is declared on (Express only), for mount composition.
    receiver: Option<String>,
}

/// Add framework routes for `path` to `result`.
pub(super) fn add_framework_routes(path: &str, content: &str, result: &mut ExtractionResult) {
    let lower = path.to_lowercase();
    let mut routes = Vec::new();
    if lower.ends_with(".py") {
        routes.extend(fastapi_routes(content));
        routes.extend(flask_routes(content));
    } else if [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"]
        .iter()
        .any(|e| lower.ends_with(e))
    {
        let (express, links) = express_routes(content);
        routes.extend(express);
        result.refs.extend(links);
        routes.extend(nestjs_routes(content));
        routes.extend(next_pages_api_routes(path, content, result));
    }
    if routes.is_empty() {
        return;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for r in routes {
        let name = format!("{} {}", r.method, r.path);
        let n = seen.entry(name.clone()).or_default();
        *n += 1;
        let qualified = if *n == 1 { name.clone() } else { format!("{} #{}", name, n) };
        let mut sym = ExtractedSymbol::simple("route", &name, &qualified, r.line, String::new());
        sym.signature = Some(format!("{} -> {}", name, r.handler));
        sym.body = format!("{}:{}", r.framework, sym.signature.clone().unwrap_or_default());
        sym.visibility = Some("public".to_string());
        // A route is not a module export (no `exports` edge).
        sym.is_exported = false;
        result.refs.push(ExtractedRef {
            kind: "references".to_string(),
            from: qualified,
            from_kind: "route".to_string(),
            target_name: r.handler,
            qualifier: Some(format!("framework:{}", r.framework)),
            line: r.line,
            col: 0,
        });
        if let Some(receiver) = r.receiver {
            result.refs.push(link_ref(LINK_EXPRESS_ROUTE, &sym.qualified_name, &receiver, None, r.line));
        }
        result.symbols.push(sym);
    }
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid route regex"))
}

/// `/api` + `/users/{id}` -> `/api/users/{id}`.
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

/// 1-based line of a byte offset.
fn line_of(content: &str, offset: usize) -> usize {
    content[..offset.min(content.len())].matches('\n').count() + 1
}

/// Arguments between the parenthesis opening at `open` and its match (strings respected).
fn balanced_args(text: &str, open: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
        } else {
            match b {
                b'"' | b'\'' | b'`' => quote = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&text[open + 1..i]);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Top-level comma-separated arguments.
fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    let mut escaped = false;
    for ch in args.chars() {
        if let Some(q) = quote {
            cur.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' | '`' => {
                quote = Some(ch);
                cur.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// A plain string literal (`'x'`, `"x"`, `` `x` `` without interpolation, Python `f`/`r` prefixes
/// without placeholders).
fn string_literal(arg: &str) -> Option<String> {
    let a = arg.trim();
    let a = a.trim_start_matches(['r', 'R', 'u', 'U', 'b', 'B']);
    let mut chars = a.chars();
    let q = chars.next()?;
    if !matches!(q, '"' | '\'' | '`') || a.len() < 2 || !a.ends_with(q) {
        return None;
    }
    let inner = &a[1..a.len() - 1];
    if inner.contains("${") || inner.contains(q) {
        return None;
    }
    Some(inner.to_string())
}

fn keyword_arg(args: &[String], key: &str) -> Option<String> {
    args.iter().find_map(|a| {
        let (k, v) = a.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_alphabetic() || f == '_' || f == '$')
        && c.all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
}

// ---------------------------------------------------------------------------
// Express
// ---------------------------------------------------------------------------

fn link_ref(kind: &str, from: &str, target: &str, detail: Option<String>, line: usize) -> ExtractedRef {
    ExtractedRef {
        kind: kind.to_string(),
        from: from.to_string(),
        from_kind: String::new(),
        target_name: target.to_string(),
        qualifier: detail,
        line,
        col: 0,
    }
}

const EXPRESS_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "options", "head", "all"];

fn express_method(m: &str) -> String {
    if m == "all" {
        "ALL".to_string()
    } else {
        m.to_uppercase()
    }
}

/// Names a file binds from a relative module (`import a from './a'`, `import { b } from './b'`,
/// `const c = require('./c')`): routers it may mount onto or re-export.
fn relative_imports(content: &str) -> Vec<String> {
    static DEFAULT: OnceLock<Regex> = OnceLock::new();
    static NAMED: OnceLock<Regex> = OnceLock::new();
    static REQUIRE: OnceLock<Regex> = OnceLock::new();
    let mut out: Vec<String> = Vec::new();
    for c in re(&DEFAULT, r#"import\s+([A-Za-z_$][\w$]*)\s*(?:,\s*\{[^}]*\}\s*)?from\s*['"]\.{1,2}/"#).captures_iter(content) {
        out.push(c[1].to_string());
    }
    for c in re(&NAMED, r#"import\s*(?:[A-Za-z_$][\w$]*\s*,\s*)?\{([^}]*)\}\s*from\s*['"]\.{1,2}/"#).captures_iter(content) {
        for item in c[1].split(',') {
            let local = item.rsplit(" as ").next().unwrap_or("").trim();
            if is_identifier(local) {
                out.push(local.to_string());
            }
        }
    }
    for c in re(&REQUIRE, r#"(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*require\(\s*['"]\.{1,2}/"#).captures_iter(content) {
        out.push(c[1].to_string());
    }
    out
}

/// Arguments of a call, with array literals flattened (`[mw, router]` -> `mw`, `router`).
fn flattened_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for a in args {
        let t = a.trim();
        if t.starts_with('[') && t.ends_with(']') {
            out.extend(flattened_args(&split_args(&t[1..t.len() - 1])));
        } else {
            out.push(t.to_string());
        }
    }
    out
}

/// Express routes of a file, plus the link facts that let the build compose mounted routers
/// across files: the receiver of each route, each `x.use('/prefix', router)` mount and each
/// exported router receiver.
///
/// Recognised: `app.METHOD(path, ..., handler)`, `router.route(path).get(h).post(h2)` chains,
/// `parent.use(path | [paths], mw?, router | [mw, router] | require('./r'))` (also on a router
/// imported from another module), routers created by `express()`, `express.Router()`,
/// `Router()`, `new Router()`, `require('express').Router()` or an aliased `Router` import, and
/// routers exported by `export default`, `export const`, `export { a as b }`,
/// `module.exports = r`, `module.exports = { a, b: c }` or `exports.x = r`.
fn express_routes(content: &str) -> (Vec<Route>, Vec<ExtractedRef>) {
    static IMPORT: OnceLock<Regex> = OnceLock::new();
    static LOCAL_DEFAULT: OnceLock<Regex> = OnceLock::new();
    static LOCAL_STAR: OnceLock<Regex> = OnceLock::new();
    static LOCAL_REQUIRE: OnceLock<Regex> = OnceLock::new();
    static NAMED_IMPORT: OnceLock<Regex> = OnceLock::new();
    static NAMED_REQUIRE: OnceLock<Regex> = OnceLock::new();
    static RECEIVER: OnceLock<Regex> = OnceLock::new();
    static RECEIVER_REQUIRE: OnceLock<Regex> = OnceLock::new();
    static CALL: OnceLock<Regex> = OnceLock::new();
    static ROUTE: OnceLock<Regex> = OnceLock::new();
    static CHAIN: OnceLock<Regex> = OnceLock::new();
    static USE: OnceLock<Regex> = OnceLock::new();
    static REQUIRE: OnceLock<Regex> = OnceLock::new();
    static EXPORT_DEFAULT: OnceLock<Regex> = OnceLock::new();
    static EXPORT_DECL: OnceLock<Regex> = OnceLock::new();
    static EXPORT_LIST: OnceLock<Regex> = OnceLock::new();
    static CJS_EXPORT: OnceLock<Regex> = OnceLock::new();
    static CJS_OBJECT: OnceLock<Regex> = OnceLock::new();
    if !re(&IMPORT, r#"(?:from\s+['"]express['"]|require\(\s*['"]express['"]\s*\))"#).is_match(content) {
        return (Vec::new(), Vec::new());
    }
    // Local names of the express module and of its `Router` factory.
    let mut express_locals: Vec<String> = vec!["express".into()];
    for c in re(&LOCAL_DEFAULT, r#"import\s+([A-Za-z_$][\w$]*)\s*(?:,\s*\{[^}]*\}\s*)?from\s*['"]express['"]"#).captures_iter(content) {
        express_locals.push(c[1].to_string());
    }
    for c in re(&LOCAL_STAR, r#"import\s*\*\s*as\s+([A-Za-z_$][\w$]*)\s+from\s*['"]express['"]"#).captures_iter(content) {
        express_locals.push(c[1].to_string());
    }
    for c in re(&LOCAL_REQUIRE, r#"(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*require\(\s*['"]express['"]\s*\)\s*(?:;|\n|$)"#)
        .captures_iter(content)
    {
        express_locals.push(c[1].to_string());
    }
    let mut router_ctors: Vec<String> = vec!["Router".into()];
    for c in re(&NAMED_IMPORT, r#"import\s*(?:[A-Za-z_$][\w$]*\s*,\s*)?\{([^}]*)\}\s*from\s*['"]express['"]"#).captures_iter(content) {
        for item in c[1].split(',') {
            if let Some((name, local)) = item.split_once(" as ") {
                if name.trim() == "Router" {
                    router_ctors.push(local.trim().to_string());
                }
            }
        }
    }
    for c in re(&NAMED_REQUIRE, r#"(?:const|let|var)\s*\{([^}]*)\}\s*=\s*require\(\s*['"]express['"]\s*\)"#).captures_iter(content) {
        for item in c[1].split(',') {
            if let Some((name, local)) = item.split_once(':') {
                if name.trim() == "Router" {
                    router_ctors.push(local.trim().to_string());
                }
            }
        }
    }
    let is_ctor = |callee: &str| {
        express_locals.iter().any(|e| callee == e || callee == format!("{}.Router", e))
            || router_ctors.iter().any(|r| r == callee)
    };
    let mut receivers: Vec<String> = vec!["app".into(), "router".into()];
    let mut declared: Vec<String> = Vec::new();
    for c in re(
        &RECEIVER,
        r"(?m)(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::\s*[\w.<>]+\s*)?=\s*(?:new\s+)?([A-Za-z_$][\w$]*(?:\.Router)?)\s*\(",
    )
    .captures_iter(content)
    {
        if is_ctor(&c[2]) {
            receivers.push(c[1].to_string());
            declared.push(c[1].to_string());
        }
    }
    for c in re(
        &RECEIVER_REQUIRE,
        r#"(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:new\s+)?require\(\s*['"]express['"]\s*\)\s*(?:\.\s*Router\s*)?\("#,
    )
    .captures_iter(content)
    {
        receivers.push(c[1].to_string());
        declared.push(c[1].to_string());
    }
    let imported = relative_imports(content);
    let mut out = Vec::new();
    let mut links = Vec::new();
    // `receiver.METHOD(path, ..., handler)`.
    for c in re(&CALL, r"\b([A-Za-z_$][\w$]*)\s*\.\s*(get|post|put|patch|delete|options|head|all)\s*\(").captures_iter(content) {
        if !receivers.iter().any(|r| r == &c[1]) {
            continue;
        }
        let m = c.get(0).unwrap();
        let Some(args) = balanced_args(content, m.end() - 1) else { continue };
        let args = split_args(args);
        if args.len() < 2 {
            continue;
        }
        let Some(path) = string_literal(&args[0]) else { continue };
        // The handler is the last argument; earlier ones are middleware.
        let handler = args.last().unwrap().trim().to_string();
        if !is_identifier(&handler) {
            continue;
        }
        out.push(Route {
            framework: "express",
            method: express_method(&c[2]),
            path: join_route("", &path),
            handler,
            line: line_of(content, m.start()),
            receiver: Some(c[1].to_string()),
        });
    }
    // `receiver.route(path).get(h).post(mw, h2)` chains.
    for c in re(&ROUTE, r"\b([A-Za-z_$][\w$]*)\s*\.\s*route\s*\(").captures_iter(content) {
        if !receivers.iter().any(|r| r == &c[1]) {
            continue;
        }
        let m = c.get(0).unwrap();
        let open = m.end() - 1;
        let Some(args) = balanced_args(content, open) else { continue };
        let Some(path) = split_args(args).first().and_then(|a| string_literal(a)) else { continue };
        let mut cursor = open + args.len() + 2;
        while let Some(link) = re(&CHAIN, r"^\s*\.\s*([A-Za-z_$][\w$]*)\s*\(").captures(&content[cursor..]) {
            let method = link[1].to_string();
            let call_open = cursor + link.get(0).unwrap().end() - 1;
            let Some(call_args) = balanced_args(content, call_open) else { break };
            let next = call_open + call_args.len() + 2;
            if EXPRESS_METHODS.contains(&method.as_str()) {
                let handler = split_args(call_args).last().map(|h| h.trim().to_string()).unwrap_or_default();
                if is_identifier(&handler) {
                    out.push(Route {
                        framework: "express",
                        method: express_method(&method),
                        path: join_route("", &path),
                        handler,
                        line: line_of(content, cursor + link.get(1).unwrap().start()),
                        receiver: Some(c[1].to_string()),
                    });
                }
            }
            cursor = next;
        }
    }
    // Mounts: `parent.use('/prefix', mw, router)`, `parent.use(router)`,
    // `parent.use(['/a', '/b'], [mw, router])`, `parent.use('/p', require('./routes'))`. Every
    // identifier argument is recorded; the build keeps only those that resolve to a router.
    for c in re(&USE, r"\b([A-Za-z_$][\w$]*)\s*\.\s*use\s*\(").captures_iter(content) {
        if !receivers.iter().any(|r| r == &c[1]) && !imported.iter().any(|r| r == &c[1]) {
            continue;
        }
        let m = c.get(0).unwrap();
        let Some(args) = balanced_args(content, m.end() - 1) else { continue };
        let args = split_args(args);
        let Some(first) = args.first() else { continue };
        let first = first.trim();
        let (prefixes, rest): (Vec<String>, &[String]) = if let Some(p) = string_literal(first) {
            (vec![p], &args[1..])
        } else if first.starts_with('[') && first.ends_with(']') {
            let items = split_args(&first[1..first.len() - 1]);
            let paths: Vec<String> = items.iter().filter_map(|a| string_literal(a)).collect();
            if !paths.is_empty() && paths.len() == items.len() {
                (paths, &args[1..])
            } else {
                (vec![String::new()], &args[..])
            }
        } else {
            (vec![String::new()], &args[..])
        };
        if rest.is_empty() {
            continue;
        }
        let line = line_of(content, m.start());
        for target in flattened_args(rest) {
            let target = if is_identifier(&target) {
                target
            } else if let Some(r) = re(&REQUIRE, r#"^require\(\s*['"]([^'"]+)['"]\s*\)$"#).captures(target.trim()) {
                format!("require:{}", &r[1])
            } else {
                continue;
            };
            for prefix in &prefixes {
                links.push(link_ref(LINK_EXPRESS_MOUNT, &c[1], &target, Some(join_route("", prefix)), line));
            }
        }
    }
    // Exported routers (declared here, or imported and re-exported).
    let mut exports: Vec<(String, String, usize)> = Vec::new();
    for c in re(&EXPORT_DEFAULT, r"(?m)^\s*export\s+default\s+([A-Za-z_$][\w$]*)\s*;?\s*$").captures_iter(content) {
        exports.push(("default".into(), c[1].to_string(), c.get(0).unwrap().start()));
    }
    for c in re(
        &EXPORT_DECL,
        r"(?m)^\s*export\s+(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::\s*[\w.<>]+\s*)?=",
    )
    .captures_iter(content)
    {
        exports.push((c[1].to_string(), c[1].to_string(), c.get(0).unwrap().start()));
    }
    for c in re(&EXPORT_LIST, r"(?m)^\s*export\s*\{([^}]*)\}\s*;?\s*$").captures_iter(content) {
        for item in c[1].split(',') {
            let item = item.trim();
            let (local, exported) = match item.split_once(" as ") {
                Some((l, e)) => (l.trim(), e.trim()),
                None => (item, item),
            };
            exports.push((exported.to_string(), local.to_string(), c.get(0).unwrap().start()));
        }
    }
    for c in re(
        &CJS_EXPORT,
        r"(?m)^\s*(?:module\.exports|exports)(?:\.([A-Za-z_$][\w$]*))?\s*=\s*([A-Za-z_$][\w$]*)\s*;?\s*$",
    )
    .captures_iter(content)
    {
        let from = c.get(1).map(|m| m.as_str().to_string()).unwrap_or_else(|| "default".to_string());
        exports.push((from, c[2].to_string(), c.get(0).unwrap().start()));
    }
    for c in re(&CJS_OBJECT, r"(?m)^\s*module\.exports\s*=\s*\{([^}]*)\}\s*;?\s*$").captures_iter(content) {
        for item in c[1].split(',') {
            let item = item.trim();
            let (exported, local) = match item.split_once(':') {
                Some((e, l)) => (e.trim(), l.trim()),
                None => (item, item),
            };
            if is_identifier(exported) && is_identifier(local) {
                exports.push((exported.to_string(), local.to_string(), c.get(0).unwrap().start()));
            }
        }
    }
    for (from, receiver, at) in exports {
        if declared.contains(&receiver) || imported.contains(&receiver) {
            links.push(link_ref(LINK_EXPRESS_EXPORT, &from, &receiver, None, line_of(content, at)));
        }
    }
    (out, links)
}

// ---------------------------------------------------------------------------
// Python: logical lines without comments / docstrings
// ---------------------------------------------------------------------------

/// Python source as logical lines (bracket continuations joined), comments removed and
/// triple-quoted strings blanked. Each entry carries its 1-based first physical line.
fn python_logical_lines(content: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut in_triple: Option<&str> = None;
    for (i, raw) in content.lines().enumerate() {
        let mut line = String::new();
        let mut rest = raw;
        // Blank triple-quoted regions.
        loop {
            if let Some(q) = in_triple {
                match rest.find(q) {
                    Some(p) => {
                        rest = &rest[p + 3..];
                        in_triple = None;
                    }
                    None => {
                        break;
                    }
                }
            } else {
                let p1 = rest.find("\"\"\"");
                let p2 = rest.find("'''");
                let (p, q) = match (p1, p2) {
                    (Some(a), Some(b)) if b < a => (Some(b), "'''"),
                    (Some(a), _) => (Some(a), "\"\"\""),
                    (None, Some(b)) => (Some(b), "'''"),
                    (None, None) => (None, ""),
                };
                match p {
                    Some(p) => {
                        line.push_str(&rest[..p]);
                        rest = &rest[p + 3..];
                        in_triple = Some(q);
                    }
                    None => {
                        line.push_str(rest);
                        break;
                    }
                }
            }
        }
        // Strip a trailing comment (outside strings).
        let mut cleaned = String::new();
        let mut quote: Option<char> = None;
        for ch in line.chars() {
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
            } else if ch == '"' || ch == '\'' {
                quote = Some(ch);
            } else if ch == '#' {
                break;
            } else if matches!(ch, '(' | '[' | '{') {
                depth += 1;
            } else if matches!(ch, ')' | ']' | '}') {
                depth -= 1;
            }
            cleaned.push(ch);
        }
        if buf.is_empty() {
            start = i + 1;
            buf = cleaned;
        } else {
            buf.push(' ');
            buf.push_str(cleaned.trim());
        }
        if depth <= 0 {
            depth = 0;
            out.push((start, std::mem::take(&mut buf)));
        }
    }
    if !buf.is_empty() {
        out.push((start, buf));
    }
    out
}

/// Pending decorators resolve to the next `def`; `None` handler means "not yet seen".
fn bind_python_handlers(
    lines: &[(usize, String)],
    framework: &'static str,
    mut decorator: impl FnMut(&str) -> Option<Vec<(String, String)>>,
) -> Vec<Route> {
    static DEF: OnceLock<Regex> = OnceLock::new();
    let def = re(&DEF, r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)\s*\(");
    let mut out = Vec::new();
    let mut pending: Vec<(usize, String, String)> = Vec::new();
    for (line_no, text) in lines {
        if let Some(found) = decorator(text) {
            for (m, p) in found {
                pending.push((*line_no, m, p));
            }
            continue;
        }
        if pending.is_empty() {
            continue;
        }
        let t = text.trim();
        if t.is_empty() || t.starts_with('@') {
            continue;
        }
        if let Some(c) = def.captures(text) {
            for (l, m, p) in pending.drain(..) {
                out.push(Route {
                    framework,
                    method: m,
                    path: p,
                    handler: c[1].to_string(),
                    line: l,
                    receiver: None,
                });
            }
        }
        pending.clear();
    }
    out
}

fn python_receivers(lines: &[(usize, String)], ctor: &Regex, prefix_key: &str) -> HashMap<String, String> {
    let mut receivers = HashMap::new();
    for (_, text) in lines {
        if let Some(c) = ctor.captures(text) {
            let open = c.get(0).unwrap().end() - 1;
            let prefix = balanced_args(text, open)
                .map(split_args)
                .and_then(|a| keyword_arg(&a, prefix_key))
                .and_then(|v| string_literal(&v))
                .unwrap_or_default();
            receivers.insert(c[1].to_string(), prefix);
        }
    }
    receivers
}

/// Names imported with `from <module> import a, b as c` (not from the framework itself).
fn python_imported_names(lines: &[(usize, String)], framework: &str) -> Vec<String> {
    static FROM: OnceLock<Regex> = OnceLock::new();
    let from = re(&FROM, r"^\s*from\s+([\w.]+)\s+import\s+(.+)$");
    let mut out = Vec::new();
    for (_, text) in lines {
        let Some(c) = from.captures(text) else { continue };
        if c[1].split('.').next() == Some(framework) {
            continue;
        }
        for part in c[2].replace(['(', ')'], "").split(',') {
            let words: Vec<&str> = part.split_whitespace().collect();
            match words.as_slice() {
                [name] => out.push(name.to_string()),
                [_, "as", alias] => out.push(alias.to_string()),
                _ => {}
            }
        }
    }
    out
}

/// `methods=` of a route decorator: `Ok(None)` when absent, `Ok(Some(..))` for a literal
/// list/tuple of plain method strings, `Err(())` when present but not statically readable
/// (`methods=METHODS`, an expression, a non-string element). An unreadable value skips the
/// route rather than guessing a method.
fn declared_methods(args: &[String]) -> Result<Option<Vec<String>>, ()> {
    let Some(v) = keyword_arg(args, "methods") else { return Ok(None) };
    let v = v.trim();
    let inner = match (v.chars().next(), v.chars().last()) {
        (Some('['), Some(']')) | (Some('('), Some(')')) => &v[1..v.len() - 1],
        _ => return Err(()),
    };
    let mut ms = Vec::new();
    for m in split_args(inner) {
        let lit = string_literal(&m).ok_or(())?;
        if lit.is_empty() || !lit.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(());
        }
        ms.push(lit.to_uppercase());
    }
    if ms.is_empty() {
        return Err(());
    }
    Ok(Some(ms))
}

// ---------------------------------------------------------------------------
// FastAPI
// ---------------------------------------------------------------------------

fn fastapi_routes(content: &str) -> Vec<Route> {
    static IMPORT: OnceLock<Regex> = OnceLock::new();
    static CTOR: OnceLock<Regex> = OnceLock::new();
    static DECO: OnceLock<Regex> = OnceLock::new();
    let lines = python_logical_lines(content);
    let ctor = re(
        &CTOR,
        r"^\s*([A-Za-z_]\w*)\s*(?::\s*[\w.\[\]]+)?\s*=\s*(?:fastapi\.)?(?:FastAPI|APIRouter)\s*\(",
    );
    let mut receivers = python_receivers(&lines, ctor, "prefix");
    let imports_fastapi = re(&IMPORT, r"(?m)^\s*(?:from\s+fastapi(?:\.[\w.]+)?\s+import\s|import\s+fastapi\b)")
        .is_match(content);
    if receivers.is_empty() && !imports_fastapi {
        return Vec::new();
    }
    // Routers declared in another module of a FastAPI project.
    if imports_fastapi {
        for n in python_imported_names(&lines, "fastapi") {
            receivers.entry(n).or_default();
        }
    }
    let deco = re(
        &DECO,
        r"^\s*@([A-Za-z_]\w*)\.(get|post|put|patch|delete|options|head|trace|api_route)\s*\(",
    );
    bind_python_handlers(&lines, "fastapi", |text| {
        let c = deco.captures(text)?;
        let prefix = receivers.get(&c[1])?;
        let open = c.get(0).unwrap().end() - 1;
        let args = split_args(balanced_args(text, open)?);
        let path = args
            .first()
            .and_then(|a| string_literal(a))
            .or_else(|| keyword_arg(&args, "path").and_then(|v| string_literal(&v)))?;
        let methods = if &c[2] == "api_route" {
            // `api_route` has no default method worth guessing.
            match declared_methods(&args) {
                Ok(Some(ms)) => ms,
                _ => return Some(Vec::new()),
            }
        } else {
            vec![c[2].to_uppercase()]
        };
        Some(methods.into_iter().map(|m| (m, join_route(prefix, &path))).collect())
    })
}

// ---------------------------------------------------------------------------
// Flask
// ---------------------------------------------------------------------------

fn flask_routes(content: &str) -> Vec<Route> {
    static IMPORT: OnceLock<Regex> = OnceLock::new();
    static CTOR: OnceLock<Regex> = OnceLock::new();
    static DECO: OnceLock<Regex> = OnceLock::new();
    let lines = python_logical_lines(content);
    let ctor = re(
        &CTOR,
        r"^\s*([A-Za-z_]\w*)\s*(?::\s*[\w.\[\]]+)?\s*=\s*(?:flask\.)?(?:Flask|Blueprint)\s*\(",
    );
    let mut receivers = python_receivers(&lines, ctor, "url_prefix");
    let imports_flask =
        re(&IMPORT, r"(?m)^\s*(?:from\s+flask(?:\.[\w.]+)?\s+import\s|import\s+flask\b)").is_match(content);
    if receivers.is_empty() && !imports_flask {
        return Vec::new();
    }
    if imports_flask {
        for n in python_imported_names(&lines, "flask") {
            receivers.entry(n).or_default();
        }
    }
    let deco = re(
        &DECO,
        r"^\s*@([A-Za-z_]\w*)\.(route|get|post|put|patch|delete|options|head)\s*\(",
    );
    bind_python_handlers(&lines, "flask", |text| {
        let c = deco.captures(text)?;
        let prefix = receivers.get(&c[1])?;
        let open = c.get(0).unwrap().end() - 1;
        let args = split_args(balanced_args(text, open)?);
        let path = args
            .first()
            .and_then(|a| string_literal(a))
            .or_else(|| keyword_arg(&args, "rule").and_then(|v| string_literal(&v)))?;
        let methods = if &c[2] == "route" {
            // `@app.route("/x")` means GET; an unreadable `methods=` is skipped, not guessed.
            match declared_methods(&args) {
                Ok(Some(ms)) => ms,
                Ok(None) => vec!["GET".into()],
                Err(()) => return Some(Vec::new()),
            }
        } else {
            vec![c[2].to_uppercase()]
        };
        Some(methods.into_iter().map(|m| (m, join_route(prefix, &path))).collect())
    })
}

// ---------------------------------------------------------------------------
// NestJS
// ---------------------------------------------------------------------------

/// `content` with every comment character replaced by a space (newlines kept, byte offsets
/// unchanged), so scanning sees comment-free code. String literals are kept verbatim:
/// decorator arguments live in them.
fn blank_comments(content: &str) -> String {
    #[derive(PartialEq)]
    enum S {
        Code,
        Line,
        Block,
        Str(char),
    }
    let mut out = String::with_capacity(content.len());
    let mut state = S::Code;
    let mut chars = content.chars().peekable();
    let mut prev = '\0';
    while let Some(ch) = chars.next() {
        let next = chars.peek().copied();
        match state {
            S::Code => {
                if ch == '/' && next == Some('/') {
                    state = S::Line;
                    out.push_str("  ");
                    chars.next();
                    prev = '\0';
                    continue;
                }
                if ch == '/' && next == Some('*') {
                    state = S::Block;
                    out.push_str("  ");
                    chars.next();
                    prev = '\0';
                    continue;
                }
                if matches!(ch, '"' | '\'' | '`') {
                    state = S::Str(ch);
                }
                out.push(ch);
            }
            S::Line => {
                if ch == '\n' {
                    state = S::Code;
                    out.push('\n');
                } else {
                    out.extend(std::iter::repeat_n(' ', ch.len_utf8()));
                }
            }
            S::Block => {
                if ch == '*' && next == Some('/') {
                    state = S::Code;
                    out.push_str("  ");
                    chars.next();
                    prev = '\0';
                    continue;
                }
                if ch == '\n' {
                    out.push('\n');
                } else {
                    out.extend(std::iter::repeat_n(' ', ch.len_utf8()));
                }
            }
            S::Str(q) => {
                if ch == q && prev != '\\' {
                    state = S::Code;
                }
                out.push(ch);
            }
        }
        prev = if prev == '\\' && ch == '\\' { '\0' } else { ch };
    }
    out
}

/// The method a decorator applies to: skip whitespace, further decorators (`@X`, `@X(...)`)
/// and modifiers from byte `from`, then take the identifier followed by `(` or `<`.
fn find_handler_name(text: &str, from: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut i = from;
    'scan: loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'@' {
            i += 1;
            while i < bytes.len() && (ident(bytes[i]) || bytes[i] == b'.') {
                i += 1;
            }
            if bytes.get(i) == Some(&b'(') {
                let args = balanced_args(text, i)?;
                i += args.len() + 2;
            }
            continue;
        }
        for kw in ["public", "private", "protected", "static", "readonly", "override", "async"] {
            if text[i..].starts_with(kw) && bytes.get(i + kw.len()).is_some_and(|b| b.is_ascii_whitespace()) {
                i += kw.len();
                continue 'scan;
            }
        }
        let start = i;
        while i < bytes.len() && ident(bytes[i]) {
            i += 1;
        }
        if start == i {
            return None;
        }
        let name = &text[start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        return matches!(bytes.get(i), Some(b'(') | Some(b'<')).then(|| name.to_string());
    }
}

/// Path prefix of `@Controller(...)` args; `None` when it cannot be read statically (a
/// constant, an array, `{ path: CONST }`), in which case the controller's routes are skipped.
fn controller_prefix(args: Option<&str>) -> Option<String> {
    let args = args?.trim();
    if args.is_empty() {
        return Some(String::new());
    }
    let first = split_args(args).into_iter().next().unwrap_or_default();
    if let Some(p) = string_literal(&first) {
        return Some(p);
    }
    if first.starts_with('{') {
        let inner = first.trim_start_matches('{').trim_end_matches('}');
        let kv = split_args(inner);
        let path = kv.iter().find_map(|p| {
            let (k, v) = p.split_once(':')?;
            (k.trim() == "path").then(|| v.to_string())
        });
        return match path {
            None => Some(String::new()),
            Some(v) => string_literal(&v),
        };
    }
    None
}

fn nestjs_routes(content: &str) -> Vec<Route> {
    static CONTROLLER: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static VERB: OnceLock<Regex> = OnceLock::new();
    if !content.contains("@nestjs/common") {
        return Vec::new();
    }
    let blanked = blank_comments(content);
    let content = blanked.as_str();
    let controller = re(&CONTROLLER, r"@Controller\s*\(");
    let class = re(&CLASS, r"(?m)^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)");
    let verb = re(&VERB, r"^\s*@(Get|Post|Put|Patch|Delete|Options|Head|All)\s*\(");
    let mut out = Vec::new();
    for c in controller.find_iter(content) {
        let open = c.end() - 1;
        let prefix = controller_prefix(balanced_args(content, open));
        let Some(cls) = class.captures(&content[c.end()..]) else { continue };
        let Some(prefix) = prefix else { continue };
        let class_name = cls[1].to_string();
        let body_start = c.end() + cls.get(0).unwrap().end();
        let Some(brace) = content[body_start..].find('{').map(|p| body_start + p) else { continue };
        // Class body extent by brace matching.
        let mut depth = 0i32;
        let mut end = content.len();
        for (i, ch) in content[brace..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = brace + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let mut offset = brace;
        for text in content[brace..end].split_inclusive('\n') {
            let line_start = offset;
            offset += text.len();
            let Some(v) = verb.captures(text) else { continue };
            let open = v.get(0).unwrap().end() - 1;
            // The decorator's arguments must close on its line.
            let Some(args) = balanced_args(text, open) else { continue };
            // An empty argument list is the controller root; an argument that is not a string
            // literal (a constant, an array, an interpolated template) cannot be read
            // statically: no route beats a wrong route.
            let path = if args.trim().is_empty() {
                String::new()
            } else {
                match split_args(args).first().and_then(|p| string_literal(p)) {
                    Some(p) => p,
                    None => continue,
                }
            };
            // The handler is the method after the decorator's closing paren, which may sit on
            // the same line (`@Get('x') list() {}`).
            let after = line_start + open + args.len() + 2;
            let Some(handler) = find_handler_name(&content[..end], after) else { continue };
            let m = if &v[1] == "All" { "ALL".to_string() } else { v[1].to_uppercase() };
            out.push(Route {
                framework: "nestjs",
                method: m,
                path: join_route(&prefix, &path),
                handler: format!("{}.{}", class_name, handler),
                line: line_of(content, line_start),
                receiver: None,
            });
        }
    }
    out
}
// ---------------------------------------------------------------------------
// Next.js Pages Router API routes (`pages/api/**`)
// ---------------------------------------------------------------------------

fn next_pages_api_routes(path: &str, content: &str, result: &ExtractionResult) -> Vec<Route> {
    let p = path.replace('\\', "/");
    let segs: Vec<&str> = p.split('/').collect();
    let Some(pos) = segs.windows(2).position(|w| w == ["pages", "api"]) else {
        return Vec::new();
    };
    let mut rest: Vec<String> = segs[pos + 1..].iter().map(|s| s.to_string()).collect();
    if let Some(last) = rest.last_mut() {
        *last = last.split('.').next().unwrap_or("").to_string();
        if last == "index" {
            rest.pop();
        }
    }
    if rest.iter().any(|s| s.starts_with('_')) {
        return Vec::new();
    }
    let route_path = format!("/{}", rest.join("/"));
    // The default export is the handler: `export default function name(..)` or
    // `export default name;`.
    static DEFAULT: OnceLock<Regex> = OnceLock::new();
    let default_name = re(
        &DEFAULT,
        r"(?m)^\s*export\s+default\s+(?:async\s+)?(?:function\s+)?([A-Za-z_$][\w$]*)",
    )
    .captures(content)
    .map(|c| c[1].to_string())
    .filter(|n| n != "function");
    let handler = result
        .symbols
        .iter()
        .filter(|s| s.kind == "function" && s.container.is_none())
        .find(|s| Some(&s.name) == default_name.as_ref());
    match handler {
        Some(h) => vec![Route {
            framework: "nextjs",
            method: "ALL".to_string(),
            path: route_path,
            handler: h.name.clone(),
            line: h.start_line,
            receiver: None,
        }],
        None => Vec::new(),
    }
}
