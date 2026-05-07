use crate::ast::language::LanguageSupport;
use crate::error::CoreError;
use crate::types::{CallEdge, FileEdge, FileEdgeKind, Symbol, SymbolKind, Visibility};
use std::path::{Component, Path, PathBuf};
use tree_sitter::{Node, Parser, Tree};

/// `LanguageSupport` implementation for TypeScript, TSX, JavaScript, and JSX.
pub struct TypeScriptLanguage {
    parser: Parser,
}

impl TypeScriptLanguage {
    /// Create a new TypeScript/TSX language support instance.
    ///
    /// `ext` should be one of `"ts"`, `"tsx"`, `"js"`, `"jsx"`.
    /// TSX and JSX files use a separate tree-sitter grammar that handles JSX syntax.
    pub fn new(ext: &str) -> Result<Self, CoreError> {
        let use_tsx = matches!(ext, "tsx" | "jsx");
        let mut parser = Parser::new();
        let lang = if use_tsx {
            tree_sitter_typescript::LANGUAGE_TSX.into()
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        };
        parser
            .set_language(&lang)
            .map_err(|e| CoreError::Parse {
                file: String::new(),
                reason: format!("failed to set TypeScript language: {e}"),
            })?;
        Ok(Self { parser })
    }
}

/// Extract the UTF-8 text of a node from source bytes.
fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Walk the AST and collect symbols.
fn collect_symbols(
    node: Node<'_>,
    src: &[u8],
    ctx: &SymbolCtx<'_>,
    parent_class: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                let is_async = node
                    .child(0)
                    .map(|c| node_text(c, src) == "async")
                    .unwrap_or(false);
                let kind = if is_async {
                    SymbolKind::AsyncFunction
                } else {
                    SymbolKind::Function
                };
                out.push(ctx.make(name, kind, Visibility::Public, node, parent_class.map(str::to_owned)));
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                out.push(ctx.make(name.clone(), SymbolKind::Class, Visibility::Public, node, parent_class.map(str::to_owned)));
                // Walk class body for methods and properties.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    collect_symbols(child, src, ctx, Some(&name), out);
                }
                return; // We already walked children above.
            }
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                out.push(ctx.make(name, SymbolKind::Interface, Visibility::Public, node, parent_class.map(str::to_owned)));
            }
        }
        "type_alias_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                out.push(ctx.make(name, SymbolKind::TypeAlias, Visibility::Public, node, parent_class.map(str::to_owned)));
            }
        }
        "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                out.push(ctx.make(name, SymbolKind::Enum, Visibility::Public, node, parent_class.map(str::to_owned)));
            }
        }
        "method_definition" => {
            // Only inside a class body.
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                // Skip constructor as it duplicates the class.
                if name != "constructor" {
                    out.push(ctx.make(name, SymbolKind::Method, Visibility::Public, node, parent_class.map(str::to_owned)));
                }
            }
        }
        "public_field_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(name_node, src).to_owned();
                out.push(ctx.make(name, SymbolKind::Property, Visibility::Public, node, parent_class.map(str::to_owned)));
            }
        }
        "lexical_declaration" => {
            // `const` / `let` at module level (parent_class == None).
            if parent_class.is_none() {
                let kind_text = node
                    .child(0)
                    .map(|c| node_text(c, src))
                    .unwrap_or("");
                let sym_kind = if kind_text == "const" {
                    SymbolKind::Const
                } else {
                    SymbolKind::Variable
                };
                // Each declarator inside the declaration.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "variable_declarator" {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            // Only emit simple identifier bindings, not destructuring.
                            if name_node.kind() == "identifier" {
                                let name = node_text(name_node, src).to_owned();
                                out.push(ctx.make(name, sym_kind.clone(), Visibility::Private, child, None));
                            }
                        }
                    }
                }
                return; // Children already handled.
            }
        }
        "export_statement" => {
            // `export default` or `export { ... }` — walk the declaration child.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_declaration"
                    | "generator_function_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "lexical_declaration" => {
                        collect_symbols(child, src, ctx, parent_class, out);
                    }
                    _ => {}
                }
            }
            return; // Already walked relevant children.
        }
        _ => {}
    }

    // Default: recurse into children (but not inside classes/interfaces, which
    // handle their own children above).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Don't recurse into class/interface bodies here; handled above.
        if matches!(child.kind(), "class_body" | "interface_body") && parent_class.is_some() {
            // We are already inside a class walk above.
        } else {
            collect_symbols(child, src, ctx, parent_class, out);
        }
    }
}

/// File-scoped context for symbol construction, reducing per-call argument count.
struct SymbolCtx<'a> {
    workspace_id: &'a str,
    file_path: &'a str,
}

impl<'a> SymbolCtx<'a> {
    fn make(
        &self,
        name: String,
        kind: SymbolKind,
        visibility: Visibility,
        node: Node<'_>,
        parent_name: Option<String>,
    ) -> Symbol {
        let pos = node.start_position();
        Symbol {
            id: 0,
            workspace_id: self.workspace_id.to_owned(),
            file_path: self.file_path.to_owned(),
            name,
            kind,
            visibility,
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
        }
    }
}

/// Walk and collect call edges.
fn collect_calls(
    node: Node<'_>,
    src: &[u8],
    file_path: &str,
    out: &mut Vec<CallEdge>,
) {
    if node.kind() == "call_expression" {
        if let Some(func_node) = node.child_by_field_name("function") {
            // Simple call: `foo()`
            let callee = match func_node.kind() {
                "identifier" => node_text(func_node, src).to_owned(),
                // `obj.method()` — take only the method name.
                "member_expression" => {
                    if let Some(prop) = func_node.child_by_field_name("property") {
                        node_text(prop, src).to_owned()
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
    } else if node.kind() == "new_expression" {
        if let Some(constr) = node.child_by_field_name("constructor") {
            let callee = node_text(constr, src).to_owned();
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

/// Walk and collect import edges.
fn collect_file_edges(
    node: Node<'_>,
    src: &[u8],
    workspace_id: &str,
    file_path: &str,
    root_path: &str,
    out: &mut Vec<FileEdge>,
) {
    match node.kind() {
        "import_statement" | "import_declaration" => {
            // `import ... from "source"`
            if let Some(source_node) = node.child_by_field_name("source") {
                let raw = node_text(source_node, src).trim_matches('"').trim_matches('\'');
                if let Some(resolved) = resolve_ts_import(raw, file_path, root_path) {
                    out.push(FileEdge {
                        workspace_id: workspace_id.to_owned(),
                        from_file: file_path.to_owned(),
                        to_file: resolved,
                        kind: FileEdgeKind::UseDecl,
                    });
                }
            }
        }
        "export_statement" => {
            // `export ... from "source"` — re-export.
            if let Some(source_node) = node.child_by_field_name("source") {
                let raw = node_text(source_node, src).trim_matches('"').trim_matches('\'');
                if let Some(resolved) = resolve_ts_import(raw, file_path, root_path) {
                    out.push(FileEdge {
                        workspace_id: workspace_id.to_owned(),
                        from_file: file_path.to_owned(),
                        to_file: resolved,
                        kind: FileEdgeKind::UseDecl,
                    });
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_file_edges(child, src, workspace_id, file_path, root_path, out);
    }
}

/// Normalize a relative `PathBuf` by resolving `.` and `..` components without
/// hitting the filesystem.  This turns `src/./Tool.ts` into `src/Tool.ts` and
/// `src/a/../b/Tool.ts` into `src/b/Tool.ts`.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {} // skip `.`
            Component::ParentDir => {
                // Pop the last component only when there is a non-root segment to pop.
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Resolve a TypeScript import path to a workspace-relative file path.
///
/// Returns `None` for external (non-relative) packages.
fn resolve_ts_import(import_path: &str, current_file: &str, root_path: &str) -> Option<String> {
    // External packages don't start with `.`
    if !import_path.starts_with('.') {
        return None;
    }

    let current_dir = Path::new(current_file)
        .parent()
        .unwrap_or(Path::new(""));
    let base = normalize_path(&current_dir.join(import_path));
    let root = Path::new(root_path);

    // Candidate extensions to try, in priority order.
    let candidates: &[&str] = &[
        "ts", "tsx", "js", "jsx",
    ];

    // Try exact path first (already has extension).
    let abs = root.join(&base);
    if abs.is_file() {
        return Some(base.to_string_lossy().replace('\\', "/"));
    }

    // Try appending extensions.
    for ext in candidates {
        let candidate = base.with_extension(ext);
        if root.join(&candidate).is_file() {
            return Some(candidate.to_string_lossy().replace('\\', "/"));
        }
    }

    // Try `path/index.<ext>`.
    for ext in candidates {
        let candidate = base.join(format!("index.{ext}"));
        if root.join(&candidate).is_file() {
            return Some(candidate.to_string_lossy().replace('\\', "/"));
        }
    }

    None
}

impl LanguageSupport for TypeScriptLanguage {
    fn parse(&mut self, src: &[u8]) -> Result<Tree, CoreError> {
        self.parser.parse(src, None).ok_or_else(|| CoreError::Parse {
            file: String::new(),
            reason: "tree-sitter returned None for TypeScript".into(),
        })
    }

    fn extract_symbols(
        &self,
        tree: &Tree,
        src: &[u8],
        workspace_id: &str,
        file_path: &str,
    ) -> Vec<Symbol> {
        let ctx = SymbolCtx { workspace_id, file_path };
        let mut out = Vec::new();
        collect_symbols(tree.root_node(), src, &ctx, None, &mut out);
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
