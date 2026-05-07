use crate::ast::language::LanguageSupport;
use crate::error::CoreError;
use crate::types::{CallEdge, FileEdge, FileEdgeKind, Symbol, SymbolKind, Visibility};
use std::path::Path;
use tree_sitter::{Node, Parser, Tree};

/// `LanguageSupport` implementation for Python.
pub struct PythonLanguage {
    parser: Parser,
}

impl PythonLanguage {
    /// Create a new Python language support instance.
    pub fn new() -> Result<Self, CoreError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .map_err(|e| CoreError::Parse {
                file: String::new(),
                reason: format!("failed to set Python language: {e}"),
            })?;
        Ok(Self { parser })
    }
}

/// Extract text of a node.
fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Return true if a `function_definition` node has an `async` keyword sibling.
fn is_async_function(node: Node<'_>, src: &[u8]) -> bool {
    // In tree-sitter-python, `async_statement` wraps `function_definition`
    // when it is an async function.
    if let Some(parent) = node.parent() {
        if parent.kind() == "decorated_definition" {
            if let Some(gp) = parent.parent() {
                if gp.kind() == "async_statement" {
                    return true;
                }
            }
        }
        if parent.kind() == "async_statement" {
            return true;
        }
    }
    // Also check for `async` as literal first child of the node itself.
    let mut cursor = node.walk();
    if let Some(child) = node.children(&mut cursor).next() {
        if child.kind() == "async" || node_text(child, src) == "async" {
            return true;
        }
    }
    false
}

fn collect_symbols(
    node: Node<'_>,
    src: &[u8],
    workspace_id: &str,
    file_path: &str,
    parent_class: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    match node.kind() {
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                let kind = if parent_class.is_some() {
                    SymbolKind::Method
                } else if is_async_function(node, src) {
                    SymbolKind::AsyncFunction
                } else {
                    SymbolKind::Function
                };
                let pos = node.start_position();
                out.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name,
                    kind,
                    visibility: Visibility::Public,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name: parent_class.map(str::to_owned),
                    signature: None,
                    body_hash: None,
                    shape_fingerprint: None,
                    shape_fingerprint_loose: None,
                    field_count: None,
                    shape_summary: None,
                    callsite_count: 0,
                    type_use_count: 0,
                });
            }
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                let pos = node.start_position();
                out.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name: name.clone(),
                    kind: SymbolKind::Class,
                    visibility: Visibility::Public,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name: parent_class.map(str::to_owned),
                    signature: None,
                    body_hash: None,
                    shape_fingerprint: None,
                    shape_fingerprint_loose: None,
                    field_count: None,
                    shape_summary: None,
                    callsite_count: 0,
                    type_use_count: 0,
                });
                // Walk class body with class name as parent.
                if let Some(body) = node.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        collect_symbols(child, src, workspace_id, file_path, Some(&name), out);
                    }
                }
                return; // Class body already walked.
            }
        }
        "decorated_definition" => {
            // Pass through to the inner definition.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if matches!(child.kind(), "function_definition" | "class_definition") {
                    collect_symbols(child, src, workspace_id, file_path, parent_class, out);
                }
            }
            return;
        }
        "expression_statement" => {
            // Module-level `name = ...` → Variable (only outside classes).
            if parent_class.is_none() {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "assignment" {
                        if let Some(left) = child.child_by_field_name("left") {
                            if left.kind() == "identifier" {
                                let name = node_text(left, src).to_owned();
                                let pos = child.start_position();
                                out.push(Symbol {
                                    id: 0,
                                    workspace_id: workspace_id.to_owned(),
                                    file_path: file_path.to_owned(),
                                    name,
                                    kind: SymbolKind::Variable,
                                    visibility: Visibility::Private,
                                    line: pos.row as u32 + 1,
                                    col: pos.column as u32,
                                    parent_name: None,
                                    signature: None,
                                    body_hash: None,
                                    shape_fingerprint: None,
                                    shape_fingerprint_loose: None,
                                    field_count: None,
                                    shape_summary: None,
                                    callsite_count: 0,
                    type_use_count: 0,
                                });
                            }
                        }
                    }
                }
                return;
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(child, src, workspace_id, file_path, parent_class, out);
    }
}

fn collect_calls(
    node: Node<'_>,
    src: &[u8],
    file_path: &str,
    out: &mut Vec<CallEdge>,
) {
    if node.kind() == "call" {
        if let Some(func_node) = node.child_by_field_name("function") {
            let callee = match func_node.kind() {
                "identifier" => node_text(func_node, src).to_owned(),
                "attribute" => {
                    // `obj.method` — take the attribute name.
                    if let Some(attr) = func_node.child_by_field_name("attribute") {
                        node_text(attr, src).to_owned()
                    } else {
                        node_text(func_node, src).to_owned()
                    }
                }
                _ => node_text(func_node, src).to_owned(),
            };
            if !callee.is_empty() {
                let pos = node.start_position();
                out.push(CallEdge {
                    workspace_id: String::new(),
                    caller_file: file_path.to_owned(),
                    caller_symbol_id: 0,
                    callee_name: callee,
                    line: pos.row as u32 + 1,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls(child, src, file_path, out);
    }
}

fn collect_file_edges(
    node: Node<'_>,
    src: &[u8],
    workspace_id: &str,
    file_path: &str,
    root_path: &str,
    out: &mut Vec<FileEdge>,
) {
    match node.kind() {
        "import_statement" => {
            // `import foo` or `import foo.bar`
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "dotted_name" {
                    let module = node_text(child, src).replace('.', "/");
                    if let Some(resolved) = resolve_python_module(
                        &module,
                        file_path,
                        root_path,
                        0,
                    ) {
                        out.push(FileEdge {
                            workspace_id: workspace_id.to_owned(),
                            from_file: file_path.to_owned(),
                            to_file: resolved,
                            kind: FileEdgeKind::UseDecl,
                        });
                    }
                }
            }
        }
        "import_from_statement" => {
            // `from .foo import bar` or `from foo import bar`
            // Count leading dots to determine relative import level.
            let raw_text = node_text(node, src);
            let (level, module_part) = parse_from_import(raw_text);
            if let Some(resolved) =
                resolve_python_module(&module_part, file_path, root_path, level)
            {
                out.push(FileEdge {
                    workspace_id: workspace_id.to_owned(),
                    from_file: file_path.to_owned(),
                    to_file: resolved,
                    kind: FileEdgeKind::UseDecl,
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_file_edges(child, src, workspace_id, file_path, root_path, out);
    }
}

/// Parse a `from ... import ...` statement text to extract the relative level and module path.
///
/// Returns `(level, module_path)` where `level` is the number of leading dots.
fn parse_from_import(text: &str) -> (usize, String) {
    // Strip `from ` prefix.
    let after_from = text.trim().strip_prefix("from").unwrap_or("").trim_start();
    // Count leading dots.
    let level = after_from.chars().take_while(|c| *c == '.').count();
    let rest = after_from[level..].trim();
    // Take the module part before ` import`.
    let module = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .replace('.', "/");
    (level, module)
}

/// Resolve a Python module path to a workspace-relative file path.
///
/// `level` is the relative import depth (0 = absolute, 1 = same package, etc.)
fn resolve_python_module(
    module: &str,
    current_file: &str,
    root_path: &str,
    level: usize,
) -> Option<String> {
    let current_dir = Path::new(current_file)
        .parent()
        .unwrap_or(Path::new(""));
    let root = Path::new(root_path);

    let base_dir: std::path::PathBuf = if level == 0 {
        // Absolute import — search from workspace root.
        Path::new("").to_path_buf()
    } else {
        // Relative: go up `level - 1` directories from current_dir.
        let mut d = current_dir.to_path_buf();
        for _ in 0..(level.saturating_sub(1)) {
            d = d.parent().unwrap_or(d.as_path()).to_path_buf();
        }
        d
    };

    if module.is_empty() {
        // `from . import something` — refers to the package init.
        let init = base_dir.join("__init__.py");
        if root.join(&init).is_file() {
            return Some(init.to_string_lossy().replace('\\', "/"));
        }
        return None;
    }

    let rel = base_dir.join(module);

    // Try `module.py`.
    let py = rel.with_extension("py");
    if root.join(&py).is_file() {
        return Some(py.to_string_lossy().replace('\\', "/"));
    }

    // Try `module/__init__.py`.
    let init = rel.join("__init__.py");
    if root.join(&init).is_file() {
        return Some(init.to_string_lossy().replace('\\', "/"));
    }

    None
}

impl LanguageSupport for PythonLanguage {
    fn parse(&mut self, src: &[u8]) -> Result<Tree, CoreError> {
        self.parser.parse(src, None).ok_or_else(|| CoreError::Parse {
            file: String::new(),
            reason: "tree-sitter returned None for Python".into(),
        })
    }

    fn extract_symbols(
        &self,
        tree: &Tree,
        src: &[u8],
        workspace_id: &str,
        file_path: &str,
    ) -> Vec<Symbol> {
        let mut out = Vec::new();
        collect_symbols(tree.root_node(), src, workspace_id, file_path, None, &mut out);
        out
    }

    fn extract_calls(
        &self,
        tree: &Tree,
        src: &[u8],
        file_path: &str,
        _file_symbols: &[Symbol],
    ) -> Vec<CallEdge> {
        let mut out = Vec::new();
        collect_calls(tree.root_node(), src, file_path, &mut out);
        out
    }

    fn extract_file_edges(
        &self,
        tree: &Tree,
        src: &[u8],
        workspace_id: &str,
        file_path: &str,
        root_path: &str,
    ) -> Vec<FileEdge> {
        let mut out = Vec::new();
        collect_file_edges(
            tree.root_node(),
            src,
            workspace_id,
            file_path,
            root_path,
            &mut out,
        );
        out
    }
}
