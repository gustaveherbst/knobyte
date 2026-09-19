use crate::graph::models::{ExtractedCall, ExtractedImport, ExtractedSymbol, ExtractedTraitImpl};
use tree_sitter::{Node as TsNode, Parser};

pub struct ExtractionResult {
    pub language: String,
    pub symbols: Vec<ExtractedSymbol>,
    pub calls: Vec<ExtractedCall>,
    pub imports: Vec<ExtractedImport>,
    pub trait_impls: Vec<ExtractedTraitImpl>,
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
            | "sql"
            | "json"
            | "yaml"
            | "yml"
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

    match ext.as_str() {
        "rs" => extract_rust(content),
        "ts" | "mts" | "cts" => extract_typescript(content, false),
        "tsx" => extract_typescript(content, true),
        "js" | "mjs" | "cjs" | "jsx" => extract_javascript(content),
        "py" => extract_python(content),
        "sql" => extract_sql(content),
        "json" | "yaml" | "yml" => extract_json_or_yaml(path, content),
        _ => None,
    }
}

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
    first_line.starts_with("async ")
        || first_line.contains(" async ")
        || first_line.starts_with("pub async ")
        || first_line.contains("pub(crate) async ")
        || first_line.contains("async fn")
}

fn extract_rust(content: &str) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = tree_sitter_rust::LANGUAGE.into();
    parser.set_language(&lang).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    let mut imports = Vec::new();
    let mut trait_impls = Vec::new();

    let lines: Vec<&str> = content.lines().collect();

    walk_rust_node(
        root,
        content,
        &lines,
        &mut symbols,
        &mut calls,
        &mut imports,
        &mut trait_impls,
        None,
        None,
    );

    Some(ExtractionResult {
        language: "rust".to_string(),
        symbols,
        calls,
        imports,
        trait_impls,
    })
}

fn walk_rust_node(
    node: TsNode,
    content: &str,
    lines: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    calls: &mut Vec<ExtractedCall>,
    imports: &mut Vec<ExtractedImport>,
    trait_impls: &mut Vec<ExtractedTraitImpl>,
    current_container: Option<&str>,
    current_trait: Option<&str>,
) {
    let kind = node.kind();

    match kind {
        "function_item" | "function_signature_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = match current_container {
                    Some(c) => format!("{}::{}", c, name),
                    None => name.clone(),
                };
                let body = node_text(node, content);
                let is_exported = node_has_child_kind(node, "visibility_modifier")
                    || current_trait.is_some()
                    || current_container.is_some();
                let is_async = is_rust_async(node, content);

                symbols.push(ExtractedSymbol {
                    kind: if current_container.is_some() {
                        "method".to_string()
                    } else {
                        "function".to_string()
                    },
                    name: name.clone(),
                    qualified_name: qualified.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_preceding_docstrings(node, lines),
                    signature: extract_first_line(node, content),
                    body,
                    is_exported,
                    is_async,
                });

                // Walk function body to find calls
                walk_children_for_calls(node, content, &qualified, calls);
            }
        }
        "struct_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                symbols.push(ExtractedSymbol {
                    kind: "struct".to_string(),
                    name: name.clone(),
                    qualified_name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_preceding_docstrings(node, lines),
                    signature: extract_first_line(node, content),
                    body: node_text(node, content),
                    is_exported: node_has_child_kind(node, "visibility_modifier"),
                    is_async: false,
                });
            }
        }
        "enum_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                symbols.push(ExtractedSymbol {
                    kind: "enum".to_string(),
                    name: name.clone(),
                    qualified_name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_preceding_docstrings(node, lines),
                    signature: extract_first_line(node, content),
                    body: node_text(node, content),
                    is_exported: node_has_child_kind(node, "visibility_modifier"),
                    is_async: false,
                });
            }
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let is_exported = node_has_child_kind(node, "visibility_modifier");
                symbols.push(ExtractedSymbol {
                    kind: "trait".to_string(),
                    name: name.clone(),
                    qualified_name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_preceding_docstrings(node, lines),
                    signature: extract_first_line(node, content),
                    body: node_text(node, content),
                    is_exported,
                    is_async: false,
                });

                if let Some(body_node) = node.child_by_field_name("body") {
                    let mut cursor = body_node.walk();
                    for child in body_node.children(&mut cursor) {
                        walk_rust_node(
                            child,
                            content,
                            lines,
                            symbols,
                            calls,
                            imports,
                            trait_impls,
                            Some(&name),
                            Some(&name),
                        );
                    }
                    return;
                }
            }
        }
        "impl_item" => {
            let trait_name = node
                .child_by_field_name("trait")
                .map(|n| node_text(n, content));
            let type_name = node
                .child_by_field_name("type")
                .map(|n| node_text(n, content));

            if let (Some(tr), Some(ty)) = (&trait_name, &type_name) {
                trait_impls.push(ExtractedTraitImpl {
                    trait_name: tr.clone(),
                    type_name: ty.clone(),
                    line: node.start_position().row + 1,
                });
            }

            let container = match (&trait_name, &type_name) {
                (Some(_tr), Some(ty)) => ty.clone(),
                (None, Some(ty)) => ty.clone(),
                _ => "impl".to_string(),
            };

            if let Some(body_node) = node.child_by_field_name("body") {
                let mut cursor = body_node.walk();
                for child in body_node.children(&mut cursor) {
                    walk_rust_node(
                        child,
                        content,
                        lines,
                        symbols,
                        calls,
                        imports,
                        trait_impls,
                        Some(&container),
                        trait_name.as_deref(),
                    );
                }
                return;
            }
        }
        "use_declaration" => {
            let text = node_text(node, content);
            imports.push(ExtractedImport {
                imported_name: text.clone(),
                source_module: text,
                line: node.start_position().row + 1,
            });
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_rust_node(
            child,
            content,
            lines,
            symbols,
            calls,
            imports,
            trait_impls,
            current_container,
            current_trait,
        );
    }
}

fn walk_children_for_calls(
    parent: TsNode,
    content: &str,
    caller_name: &str,
    calls: &mut Vec<ExtractedCall>,
) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        let kind = child.kind();
        if kind == "call_expression" {
            if let Some(func_node) = child.child_by_field_name("function") {
                let target = node_text(func_node, content);
                let target_ident = target.split("::").last().unwrap_or(&target);
                let target_ident = target_ident
                    .split('.')
                    .next_back()
                    .unwrap_or(target_ident)
                    .trim();
                let receiver = if target.contains('.') {
                    target.rsplit_once('.').map(|(r, _)| r.trim().to_string())
                } else if target.contains("::") {
                    target.rsplit_once("::").map(|(r, _)| r.trim().to_string())
                } else {
                    None
                };
                if !target_ident.is_empty() {
                    calls.push(ExtractedCall {
                        caller_name: caller_name.to_string(),
                        target_name: target_ident.to_string(),
                        receiver,
                        line: child.start_position().row + 1,
                        col: child.start_position().column,
                    });
                }
            }
        } else if kind == "method_call_expression" {
            let name_node = child.child_by_field_name("name");
            let value_node = child.child_by_field_name("value");
            if let Some(name_n) = name_node {
                let method_name = node_text(name_n, content);
                let receiver = value_node.map(|v| node_text(v, content));
                calls.push(ExtractedCall {
                    caller_name: caller_name.to_string(),
                    target_name: method_name,
                    receiver,
                    line: child.start_position().row + 1,
                    col: child.start_position().column,
                });
            }
        }
        walk_children_for_calls(child, content, caller_name, calls);
    }
}

fn extract_typescript(content: &str, is_tsx: bool) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = if is_tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    parser.set_language(&lang).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    let mut imports = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    walk_ts_node(
        root,
        content,
        &lines,
        &mut symbols,
        &mut calls,
        &mut imports,
        None,
    );

    Some(ExtractionResult {
        language: if is_tsx {
            "tsx".to_string()
        } else {
            "typescript".to_string()
        },
        symbols,
        calls,
        imports,
        trait_impls: Vec::new(),
    })
}

fn extract_javascript(content: &str) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = tree_sitter_javascript::LANGUAGE.into();
    parser.set_language(&lang).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    let mut imports = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    walk_ts_node(
        root,
        content,
        &lines,
        &mut symbols,
        &mut calls,
        &mut imports,
        None,
    );

    Some(ExtractionResult {
        language: "javascript".to_string(),
        symbols,
        calls,
        imports,
        trait_impls: Vec::new(),
    })
}

fn walk_ts_node(
    node: TsNode,
    content: &str,
    lines: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    calls: &mut Vec<ExtractedCall>,
    imports: &mut Vec<ExtractedImport>,
    current_container: Option<&str>,
) {
    let kind = node.kind();

    match kind {
        "function_declaration" | "method_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = match current_container {
                    Some(c) => format!("{}.{}", c, name),
                    None => name.clone(),
                };
                let body = node_text(node, content);
                let is_async = node_has_child_kind(node, "async");

                symbols.push(ExtractedSymbol {
                    kind: "function".to_string(),
                    name: name.clone(),
                    qualified_name: qualified.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_preceding_docstrings(node, lines),
                    signature: extract_first_line(node, content),
                    body,
                    is_exported: is_ts_exported(node),
                    is_async,
                });

                walk_ts_calls(node, content, &qualified, calls);
            }
        }
        "class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                symbols.push(ExtractedSymbol {
                    kind: "class".to_string(),
                    name: name.clone(),
                    qualified_name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_preceding_docstrings(node, lines),
                    signature: extract_first_line(node, content),
                    body: node_text(node, content),
                    is_exported: is_ts_exported(node),
                    is_async: false,
                });

                if let Some(body_node) = node.child_by_field_name("body") {
                    let mut cursor = body_node.walk();
                    for child in body_node.children(&mut cursor) {
                        walk_ts_node(child, content, lines, symbols, calls, imports, Some(&name));
                    }
                    return;
                }
            }
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                symbols.push(ExtractedSymbol {
                    kind: "interface".to_string(),
                    name: name.clone(),
                    qualified_name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_preceding_docstrings(node, lines),
                    signature: extract_first_line(node, content),
                    body: node_text(node, content),
                    is_exported: is_ts_exported(node),
                    is_async: false,
                });
            }
        }
        "import_statement" => {
            let text = node_text(node, content);
            imports.push(ExtractedImport {
                imported_name: text.clone(),
                source_module: text,
                line: node.start_position().row + 1,
            });
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ts_node(
            child,
            content,
            lines,
            symbols,
            calls,
            imports,
            current_container,
        );
    }
}

fn walk_ts_calls(parent: TsNode, content: &str, caller_name: &str, calls: &mut Vec<ExtractedCall>) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(func_node) = child.child_by_field_name("function") {
                let target = node_text(func_node, content);
                let target_ident = target.split('.').next_back().unwrap_or(&target).trim();
                let receiver = if target.contains('.') {
                    target.rsplit_once('.').map(|(r, _)| r.trim().to_string())
                } else {
                    None
                };
                if !target_ident.is_empty() {
                    calls.push(ExtractedCall {
                        caller_name: caller_name.to_string(),
                        target_name: target_ident.to_string(),
                        receiver,
                        line: child.start_position().row + 1,
                        col: child.start_position().column,
                    });
                }
            }
        }
        walk_ts_calls(child, content, caller_name, calls);
    }
}

fn is_ts_exported(node: TsNode) -> bool {
    if let Some(parent) = node.parent() {
        if parent.kind() == "export_statement" {
            return true;
        }
    }
    node_has_child_kind(node, "export")
}

fn extract_python(content: &str) -> Option<ExtractionResult> {
    let mut parser = Parser::new();
    let lang = tree_sitter_python::LANGUAGE.into();
    parser.set_language(&lang).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    let mut imports = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    walk_py_node(
        root,
        content,
        &lines,
        &mut symbols,
        &mut calls,
        &mut imports,
        None,
    );

    Some(ExtractionResult {
        language: "python".to_string(),
        symbols,
        calls,
        imports,
        trait_impls: Vec::new(),
    })
}

fn walk_py_node(
    node: TsNode,
    content: &str,
    lines: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    calls: &mut Vec<ExtractedCall>,
    imports: &mut Vec<ExtractedImport>,
    current_container: Option<&str>,
) {
    let kind = node.kind();

    match kind {
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                let qualified = match current_container {
                    Some(c) => format!("{}.{}", c, name),
                    None => name.clone(),
                };
                let body = node_text(node, content);
                let is_async = node_has_child_kind(node, "async");

                symbols.push(ExtractedSymbol {
                    kind: "function".to_string(),
                    name: name.clone(),
                    qualified_name: qualified.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_python_docstring(node, content),
                    signature: extract_first_line(node, content),
                    body,
                    is_exported: !name.starts_with('_'),
                    is_async,
                });

                walk_py_calls(node, content, &qualified, calls);
            }
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, content);
                symbols.push(ExtractedSymbol {
                    kind: "class".to_string(),
                    name: name.clone(),
                    qualified_name: name.clone(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    start_col: node.start_position().column,
                    end_col: node.end_position().column,
                    docstring: extract_python_docstring(node, content),
                    signature: extract_first_line(node, content),
                    body: node_text(node, content),
                    is_exported: !name.starts_with('_'),
                    is_async: false,
                });

                if let Some(body_node) = node.child_by_field_name("body") {
                    let mut cursor = body_node.walk();
                    for child in body_node.children(&mut cursor) {
                        walk_py_node(child, content, lines, symbols, calls, imports, Some(&name));
                    }
                    return;
                }
            }
        }
        "import_statement" | "import_from_statement" => {
            let text = node_text(node, content);
            imports.push(ExtractedImport {
                imported_name: text.clone(),
                source_module: text,
                line: node.start_position().row + 1,
            });
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_py_node(
            child,
            content,
            lines,
            symbols,
            calls,
            imports,
            current_container,
        );
    }
}

fn walk_py_calls(parent: TsNode, content: &str, caller_name: &str, calls: &mut Vec<ExtractedCall>) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() == "call" {
            if let Some(func_node) = child.child_by_field_name("function") {
                let target = node_text(func_node, content);
                let target_ident = target.split('.').next_back().unwrap_or(&target).trim();
                let receiver = if target.contains('.') {
                    target.rsplit_once('.').map(|(r, _)| r.trim().to_string())
                } else {
                    None
                };
                if !target_ident.is_empty() {
                    calls.push(ExtractedCall {
                        caller_name: caller_name.to_string(),
                        target_name: target_ident.to_string(),
                        receiver,
                        line: child.start_position().row + 1,
                        col: child.start_position().column,
                    });
                }
            }
        }
        walk_py_calls(child, content, caller_name, calls);
    }
}

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
    };

    Some(ExtractionResult {
        language: "markdown".to_string(),
        symbols: vec![symbol],
        calls: Vec::new(),
        imports: Vec::new(),
        trait_impls: Vec::new(),
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

fn extract_preceding_docstrings(node: TsNode, lines: &[&str]) -> Option<String> {
    let start_line = node.start_position().row;
    if start_line == 0 {
        return None;
    }

    let mut doc_lines = Vec::new();
    let mut cur = start_line;

    while cur > 0 {
        cur -= 1;
        let line = lines[cur].trim();
        if line.starts_with("///")
            || line.starts_with("//!")
            || line.starts_with("/**")
            || line.starts_with("*")
        {
            doc_lines.push(line);
        } else if line.is_empty() {
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
