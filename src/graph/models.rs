use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    pub identity_key: String,
    pub file_path: String,
    pub language: String,
    pub start_line: i64,
    pub end_line: i64,
    pub start_column: i64,
    pub end_column: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    pub is_exported: bool,
    pub is_async: bool,
    pub is_static: bool,
    pub is_abstract: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_hash: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub source: String,
    pub target: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub content_hash: String,
    pub language: String,
    pub size: i64,
    pub modified_at: i64,
    pub indexed_at: i64,
    pub node_count: i64,
    pub parse_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphStatus {
    pub up_to_date: bool,
    pub schema_version: i64,
    pub file_count: i64,
    pub node_count: i64,
    pub edge_count: i64,
    pub unresolved_count: i64,
    pub last_indexed: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedSymbol {
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub docstring: Option<String>,
    pub signature: Option<String>,
    /// Full declaration text. Not persisted in the extraction cache (only its hash is).
    #[serde(skip)]
    pub body: String,
    pub is_exported: bool,
    pub is_async: bool,
    /// Qualified name of the enclosing container (impl type, trait, class), if any.
    pub container: Option<String>,
    /// Declared visibility (`pub`, `pub(crate)`, `private`, `public`, `protected`, ...).
    pub visibility: Option<String>,
    /// Declared return type text, if the language/grammar exposes one.
    pub return_type: Option<String>,
    pub is_static: bool,
    pub is_abstract: bool,
}

impl ExtractedSymbol {
    /// Minimal constructor used by the simple (non tree-sitter) extractors.
    pub fn simple(kind: &str, name: &str, qualified_name: &str, line: usize, body: String) -> Self {
        Self {
            kind: kind.to_string(),
            name: name.to_string(),
            qualified_name: qualified_name.to_string(),
            start_line: line,
            end_line: line,
            start_col: 0,
            end_col: 0,
            docstring: None,
            signature: Some(qualified_name.to_string()),
            body,
            is_exported: true,
            is_async: false,
            container: None,
            visibility: None,
            return_type: None,
            is_static: false,
            is_abstract: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedCall {
    /// Qualified name of the innermost enclosing function/method ("" at module level).
    pub caller_name: String,
    /// Bare name of the called function/method.
    pub target_name: String,
    /// Receiver expression text for method calls (`self`, `this`, `user`, ...).
    pub receiver: Option<String>,
    /// Path qualifier for path calls (`Foo` in `Foo::new()`, `crate::auth` in `crate::auth::check()`).
    pub qualifier: Option<String>,
    /// Explicitly annotated type of a method call's receiver (`r` in `const r: Runner = ..;
    /// r.run()` / `(r: Runner) => r.run()`), when the extractor can see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_type: Option<String>,
    /// True for `recv.method()` style calls.
    pub is_method: bool,
    pub line: usize,
    pub col: usize,
}

/// One bound name produced by an import statement.
///
/// `use crate::auth::{validate as check, Session};` yields two imports:
/// `{module: "crate::auth", imported: "validate", local: "check"}` and
/// `{module: "crate::auth", imported: "Session", local: "Session"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedImport {
    /// Name exported by the source module (`*` for wildcard/namespace, `default` for JS default imports,
    /// empty for side-effect-only imports).
    pub imported_name: String,
    /// Module specifier / path the name comes from (`crate::auth`, `./auth`, `pkg.sub`, `..models`).
    pub source_module: String,
    /// Name bound in the importing file (alias if present). Empty for side-effect imports.
    pub local_name: String,
    /// True when the import binds the module itself (`import os`, `import * as ns`, `use crate::auth;`).
    pub is_module: bool,
    pub is_type_only: bool,
    pub is_reexport: bool,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedTraitImpl {
    pub trait_name: String,
    pub type_name: String,
    pub line: usize,
}

/// A non-call reference from a declaration to a named symbol, resolved after the whole corpus
/// is extracted (`extends`, `implements`, `instantiates`, `returns`, `type_of`, `decorates`,
/// `references`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRef {
    pub kind: String,
    /// Qualified name of the referencing declaration in the same file (empty: the file itself).
    pub from: String,
    /// Kind of the referencing declaration (empty: the file itself).
    pub from_kind: String,
    /// Bare referenced name (last path segment).
    pub target_name: String,
    /// Path or receiver qualifier written before the name (`models` in `models.User`).
    pub qualifier: Option<String>,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopedNode {
    pub node: Node,
    pub score: f64,
    pub reason: String,
}
