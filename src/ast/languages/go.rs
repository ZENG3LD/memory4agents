use crate::ast::language::LanguageSupport;
use crate::error::CoreError;
use crate::types::{CallEdge, FileEdge, FileEdgeKind, Symbol, SymbolKind, Visibility};
use tree_sitter::{Node, Parser, Tree};

/// `LanguageSupport` implementation for Go.
pub struct GoLanguage {
    parser: Parser,
}

impl GoLanguage {
    /// Create a new Go language support instance.
    pub fn new() -> Result<Self, CoreError> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|e| CoreError::Parse {
                file: String::new(),
                reason: format!("failed to set Go language: {e}"),
            })?;
        Ok(Self { parser })
    }
}

/// Extract UTF-8 text of a node.
fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn collect_symbols(
    node: Node<'_>,
    src: &[u8],
    workspace_id: &str,
    file_path: &str,
    out: &mut Vec<Symbol>,
) {
    match node.kind() {
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                let pos = node.start_position();
                out.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name,
                    kind: SymbolKind::Function,
                    visibility: Visibility::Public,
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
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                // Extract receiver type from the receiver field.
                let parent_name = node.child_by_field_name("receiver").and_then(|recv| {
                    // Receiver is a parameter list; walk to find the type.
                    let mut cursor = recv.walk();
                    for child in recv.children(&mut cursor) {
                        if child.kind() == "parameter_declaration" {
                            if let Some(type_node) = child.child_by_field_name("type") {
                                // Could be `*TypeName` or `TypeName`.
                                let type_text = node_text(type_node, src)
                                    .trim_start_matches('*')
                                    .to_owned();
                                return Some(type_text);
                            }
                        }
                    }
                    None
                });
                let pos = node.start_position();
                out.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name,
                    kind: SymbolKind::Method,
                    visibility: Visibility::Public,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
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
        "type_declaration" => {
            // Walk spec children.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(name_node, src).to_owned();
                        let kind = if let Some(type_node) = child.child_by_field_name("type") {
                            match type_node.kind() {
                                "struct_type" => SymbolKind::Struct,
                                "interface_type" => SymbolKind::Interface,
                                _ => SymbolKind::TypeAlias,
                            }
                        } else {
                            SymbolKind::TypeAlias
                        };
                        let pos = child.start_position();
                        out.push(Symbol {
                            id: 0,
                            workspace_id: workspace_id.to_owned(),
                            file_path: file_path.to_owned(),
                            name,
                            kind,
                            visibility: Visibility::Public,
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
            return; // Children handled above.
        }
        "const_declaration" => {
            // `const Name = ...` or `const (Name = ...)`.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "const_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(name_node, src).to_owned();
                        let pos = child.start_position();
                        out.push(Symbol {
                            id: 0,
                            workspace_id: workspace_id.to_owned(),
                            file_path: file_path.to_owned(),
                            name,
                            kind: SymbolKind::Const,
                            visibility: Visibility::Public,
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
            return;
        }
        "var_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "var_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(name_node, src).to_owned();
                        let pos = child.start_position();
                        out.push(Symbol {
                            id: 0,
                            workspace_id: workspace_id.to_owned(),
                            file_path: file_path.to_owned(),
                            name,
                            kind: SymbolKind::Variable,
                            visibility: Visibility::Public,
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
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(child, src, workspace_id, file_path, out);
    }
}

fn collect_calls(
    node: Node<'_>,
    src: &[u8],
    file_path: &str,
    out: &mut Vec<CallEdge>,
) {
    if node.kind() == "call_expression" {
        if let Some(func_node) = node.child_by_field_name("function") {
            let callee = match func_node.kind() {
                "identifier" => node_text(func_node, src).to_owned(),
                "selector_expression" => {
                    // `pkg.Func` style — extract the field (method/function name).
                    if let Some(field) = func_node.child_by_field_name("field") {
                        node_text(field, src).to_owned()
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
    out: &mut Vec<FileEdge>,
) {
    if node.kind() == "import_declaration" {
        // Walk import specs.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "import_spec" | "import_spec_list" => {
                    extract_import_spec(child, src, workspace_id, file_path, out);
                }
                _ => {}
            }
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_file_edges(child, src, workspace_id, file_path, out);
    }
}

fn extract_import_spec(
    node: Node<'_>,
    src: &[u8],
    workspace_id: &str,
    file_path: &str,
    out: &mut Vec<FileEdge>,
) {
    if node.kind() == "import_spec" {
        if let Some(path_node) = node.child_by_field_name("path") {
            let raw = node_text(path_node, src)
                .trim_matches('"')
                .trim_matches('`')
                .to_owned();
            if !raw.is_empty() {
                // Go import paths are package paths, not filesystem paths in phase 1.
                // Store them as-is for future resolution.
                out.push(FileEdge {
                    workspace_id: workspace_id.to_owned(),
                    from_file: file_path.to_owned(),
                    to_file: raw,
                    kind: FileEdgeKind::UseDecl,
                });
            }
        }
    } else if node.kind() == "import_spec_list" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "import_spec" {
                extract_import_spec(child, src, workspace_id, file_path, out);
            }
        }
    }
}

impl LanguageSupport for GoLanguage {
    fn parse(&mut self, src: &[u8]) -> Result<Tree, CoreError> {
        self.parser.parse(src, None).ok_or_else(|| CoreError::Parse {
            file: String::new(),
            reason: "tree-sitter returned None for Go".into(),
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
        collect_symbols(tree.root_node(), src, workspace_id, file_path, &mut out);
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
        _root_path: &str,
    ) -> Vec<FileEdge> {
        let mut out = Vec::new();
        collect_file_edges(tree.root_node(), src, workspace_id, file_path, &mut out);
        out
    }
}
