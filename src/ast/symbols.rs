use crate::ast::fingerprint::compute_shape_info;
use crate::ast::known_types::extract_type_names;
use crate::types::{PendingTypeUse, Symbol, SymbolKind, Visibility};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Tree};

/// Determine the use_kind for a symbol based on its kind.
///
/// For struct/enum/trait the entire signature is treated as a 'field' reference.
/// For fn/method/impl the param/return classification is done textually.
/// We use a simple heuristic: everything extracted from a signature is 'field'
/// for data types, and we separate param/return for functions by looking at
/// whether the type appears before or after the `->` arrow.
fn classify_uses(sym: &Symbol) -> Vec<PendingTypeUse> {
    let sig = match &sym.signature {
        Some(s) => s.as_str(),
        None => return Vec::new(),
    };

    match sym.kind {
        SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait => {
            // All type names in the signature are field-like usages.
            extract_type_names(sig)
                .into_iter()
                .map(|to_name| PendingTypeUse {
                    from_name: sym.name.clone(),
                    from_line: sym.line,
                    to_name,
                    use_kind: "field",
                })
                .collect()
        }
        SymbolKind::TypeAlias => {
            extract_type_names(sig)
                .into_iter()
                .map(|to_name| PendingTypeUse {
                    from_name: sym.name.clone(),
                    from_line: sym.line,
                    to_name,
                    use_kind: "generic",
                })
                .collect()
        }
        SymbolKind::Function
        | SymbolKind::AsyncFunction
        | SymbolKind::Method
        | SymbolKind::Impl => {
            // Split signature at `->` to separate param types from return type.
            let mut uses = Vec::new();
            if let Some(arrow_pos) = sig.find("->") {
                let params_part = &sig[..arrow_pos];
                let return_part = &sig[arrow_pos + 2..];
                for name in extract_type_names(params_part) {
                    uses.push(PendingTypeUse {
                        from_name: sym.name.clone(),
                        from_line: sym.line,
                        to_name: name,
                        use_kind: "param",
                    });
                }
                for name in extract_type_names(return_part) {
                    uses.push(PendingTypeUse {
                        from_name: sym.name.clone(),
                        from_line: sym.line,
                        to_name: name,
                        use_kind: "return",
                    });
                }
            } else {
                for name in extract_type_names(sig) {
                    uses.push(PendingTypeUse {
                        from_name: sym.name.clone(),
                        from_line: sym.line,
                        to_name: name,
                        use_kind: "param",
                    });
                }
            }
            uses
        }
        _ => Vec::new(),
    }
}

/// Extract pending type-use edges from a list of symbols (by scanning their signatures).
pub fn extract_type_uses(symbols: &[Symbol]) -> Vec<PendingTypeUse> {
    symbols.iter().flat_map(classify_uses).collect()
}

/// Extract all symbols from a parsed tree.
pub fn extract_symbols(
    tree: &Tree,
    src: &[u8],
    workspace_id: &str,
    file_path: &str,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut cursor = tree.walk();
    // Stack of enclosing context names (impl type or mod name).
    let mut parent_stack: Vec<String> = Vec::new();
    walk_node(
        &mut cursor,
        src,
        workspace_id,
        file_path,
        &mut parent_stack,
        &mut symbols,
    );
    symbols
}

fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn get_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    // Try "name" field first.
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(name_node, src).to_owned());
    }
    // Fallback: any identifier or type_identifier child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return Some(node_text(child, src).to_owned());
        }
    }
    None
}

fn get_impl_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    // For impl nodes: look for type_identifier child (the type being implemented).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_identifier" {
            return Some(node_text(child, src).to_owned());
        }
    }
    None
}

fn get_visibility(node: Node<'_>, src: &[u8]) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = node_text(child, src);
            if text.contains("crate") {
                return Visibility::Crate;
            }
            if text.contains("super") {
                return Visibility::Super;
            }
            return Visibility::Public;
        }
    }
    Visibility::Private
}

fn is_async_function(node: Node<'_>, src: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "async" {
            return true;
        }
        if child.kind() == "function_modifiers" {
            let text = node_text(child, src);
            if text.contains("async") {
                return true;
            }
        }
    }
    false
}

fn has_body(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "declaration_list" || child.kind() == "block" {
            return true;
        }
    }
    false
}

/// Normalize text: collapse whitespace runs, strip `//` and `/* */` comments.
fn normalize(text: &str) -> String {
    // Strip block comments /* ... */ (non-greedy)
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            // Line comment — skip to end of line
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Block comment — skip until */
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2; // skip */
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    // Collapse whitespace runs to single space
    let mut result = String::with_capacity(out.len());
    let mut prev_space = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(ch);
            prev_space = false;
        }
    }
    result.trim().to_owned()
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Build signature for function/impl: node text minus the block child.
fn signature_without_block(node: Node<'_>, src: &[u8]) -> String {
    let full = node_text(node, src);
    // Find block child (function body or impl body)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block" || child.kind() == "declaration_list" {
            let block_start = child.start_byte();
            let node_start = node.start_byte();
            if block_start >= node_start {
                let sig_bytes = &src[node_start..block_start];
                let sig_str = std::str::from_utf8(sig_bytes).unwrap_or("");
                return normalize(sig_str);
            }
        }
    }
    // No block found — use entire text (e.g. trait fn declaration)
    normalize(full)
}

/// Hash the block body of a node (block or declaration_list child).
fn body_hash_of_block(node: Node<'_>, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block" || child.kind() == "declaration_list" {
            let body_text = node_text(child, src);
            let norm = normalize(body_text);
            return Some(sha256_hex(&norm));
        }
    }
    None
}

/// For trait_item: collect fn signatures from declaration_list without their blocks.
fn trait_signature(node: Node<'_>, src: &[u8]) -> String {
    // Find declaration_list
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "declaration_list" {
            // Collect fn declarations without bodies
            let mut parts = Vec::new();
            let mut inner = child.walk();
            for item in child.children(&mut inner) {
                if item.kind() == "function_item" || item.kind() == "function_signature_item" {
                    let sig = signature_without_block(item, src);
                    parts.push(sig);
                }
            }
            // Prefix with the trait header (everything before the declaration_list)
            let node_start = node.start_byte();
            let decl_start = child.start_byte();
            if decl_start >= node_start {
                let header = std::str::from_utf8(&src[node_start..decl_start]).unwrap_or("");
                let header_norm = normalize(header);
                let body_part = parts.join("; ");
                return format!("{header_norm} {{ {body_part} }}");
            }
        }
    }
    // Fallback: full node text
    normalize(node_text(node, src))
}

fn walk_node(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    src: &[u8],
    workspace_id: &str,
    file_path: &str,
    parent_stack: &mut Vec<String>,
    symbols: &mut Vec<Symbol>,
) {
    let node = cursor.node();
    let node_kind = node.kind();

    let pushed = match node_kind {
        "function_item" => {
            if let Some(name) = get_name(node, src) {
                let kind = if is_async_function(node, src) {
                    SymbolKind::AsyncFunction
                } else {
                    SymbolKind::Function
                };
                let visibility = get_visibility(node, src);
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(signature_without_block(node, src));
                let body_hash = body_hash_of_block(node, src);
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name: name.clone(),
                    kind,
                    visibility,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash,
                    shape_fingerprint: None,
                    shape_fingerprint_loose: None,
                    field_count: None,
                    shape_summary: None,
                    callsite_count: 0,
                    type_use_count: 0,
                });
                parent_stack.push(name);
                true
            } else {
                false
            }
        }
        "struct_item" => {
            if let Some(name) = get_name(node, src) {
                let visibility = get_visibility(node, src);
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(normalize(node_text(node, src)));
                let shape_info = compute_shape_info(node, src, &SymbolKind::Struct);
                let shape_fingerprint = shape_info.as_ref().map(|i| i.strict.clone());
                let shape_fingerprint_loose = shape_info.as_ref().map(|i| i.loose.clone());
                let field_count = shape_info.as_ref().map(|i| i.field_count);
                let shape_summary = shape_info.map(|i| i.summary);
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name: name.clone(),
                    kind: SymbolKind::Struct,
                    visibility,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash: None,
                    shape_fingerprint,
                    shape_fingerprint_loose,
                    field_count,
                    shape_summary,
                    callsite_count: 0,
                    type_use_count: 0,
                });
                parent_stack.push(name);
                true
            } else {
                false
            }
        }
        "enum_item" => {
            if let Some(name) = get_name(node, src) {
                let visibility = get_visibility(node, src);
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(normalize(node_text(node, src)));
                let shape_info = compute_shape_info(node, src, &SymbolKind::Enum);
                let shape_fingerprint = shape_info.as_ref().map(|i| i.strict.clone());
                let shape_fingerprint_loose = shape_info.as_ref().map(|i| i.loose.clone());
                let field_count = shape_info.as_ref().map(|i| i.field_count);
                let shape_summary = shape_info.map(|i| i.summary);
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name: name.clone(),
                    kind: SymbolKind::Enum,
                    visibility,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash: None,
                    shape_fingerprint,
                    shape_fingerprint_loose,
                    field_count,
                    shape_summary,
                    callsite_count: 0,
                    type_use_count: 0,
                });
                parent_stack.push(name);
                true
            } else {
                false
            }
        }
        "trait_item" => {
            if let Some(name) = get_name(node, src) {
                let visibility = get_visibility(node, src);
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(trait_signature(node, src));
                let shape_info = compute_shape_info(node, src, &SymbolKind::Trait);
                let shape_fingerprint = shape_info.as_ref().map(|i| i.strict.clone());
                let shape_fingerprint_loose = shape_info.as_ref().map(|i| i.loose.clone());
                let field_count = shape_info.as_ref().map(|i| i.field_count);
                let shape_summary = shape_info.map(|i| i.summary);
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name: name.clone(),
                    kind: SymbolKind::Trait,
                    visibility,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash: None,
                    shape_fingerprint,
                    shape_fingerprint_loose,
                    field_count,
                    shape_summary,
                    callsite_count: 0,
                    type_use_count: 0,
                });
                parent_stack.push(name);
                true
            } else {
                false
            }
        }
        "impl_item" => {
            if let Some(name) = get_impl_name(node, src) {
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(signature_without_block(node, src));
                let body_hash = body_hash_of_block(node, src);
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name: name.clone(),
                    kind: SymbolKind::Impl,
                    visibility: Visibility::Private,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash,
                    shape_fingerprint: None,
                    shape_fingerprint_loose: None,
                    field_count: None,
                    shape_summary: None,
                    callsite_count: 0,
                    type_use_count: 0,
                });
                parent_stack.push(name);
                true
            } else {
                false
            }
        }
        "const_item" => {
            if let Some(name) = get_name(node, src) {
                let visibility = get_visibility(node, src);
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(normalize(node_text(node, src)));
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name,
                    kind: SymbolKind::Const,
                    visibility,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash: None,
                    shape_fingerprint: None,
                    shape_fingerprint_loose: None,
                    field_count: None,
                    shape_summary: None,
                    callsite_count: 0,
                    type_use_count: 0,
                });
            }
            false
        }
        "type_item" => {
            if let Some(name) = get_name(node, src) {
                let visibility = get_visibility(node, src);
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(normalize(node_text(node, src)));
                let shape_info = compute_shape_info(node, src, &SymbolKind::TypeAlias);
                let shape_fingerprint = shape_info.as_ref().map(|i| i.strict.clone());
                let shape_fingerprint_loose = shape_info.as_ref().map(|i| i.loose.clone());
                let field_count = shape_info.as_ref().map(|i| i.field_count);
                let shape_summary = shape_info.map(|i| i.summary);
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name,
                    kind: SymbolKind::TypeAlias,
                    visibility,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash: None,
                    shape_fingerprint,
                    shape_fingerprint_loose,
                    field_count,
                    shape_summary,
                    callsite_count: 0,
                    type_use_count: 0,
                });
            }
            false
        }
        "macro_definition" => {
            // macro_rules! foo — name is identifier child
            let mut cursor2 = node.walk();
            let mut macro_name: Option<String> = None;
            for child in node.children(&mut cursor2) {
                if child.kind() == "identifier" {
                    macro_name = Some(node_text(child, src).to_owned());
                    break;
                }
            }
            if let Some(name) = macro_name {
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(normalize(node_text(node, src)));
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name,
                    kind: SymbolKind::Macro,
                    visibility: Visibility::Private,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash: None,
                    shape_fingerprint: None,
                    shape_fingerprint_loose: None,
                    field_count: None,
                    shape_summary: None,
                    callsite_count: 0,
                    type_use_count: 0,
                });
            }
            false
        }
        "mod_item" => {
            if let Some(name) = get_name(node, src) {
                let visibility = get_visibility(node, src);
                let pos = node.start_position();
                let parent_name = parent_stack.last().cloned();
                let signature = Some(normalize(node_text(node, src)));
                symbols.push(Symbol {
                    id: 0,
                    workspace_id: workspace_id.to_owned(),
                    file_path: file_path.to_owned(),
                    name: name.clone(),
                    kind: SymbolKind::Module,
                    visibility,
                    line: pos.row as u32 + 1,
                    col: pos.column as u32,
                    parent_name,
                    signature,
                    body_hash: None,
                    shape_fingerprint: None,
                    shape_fingerprint_loose: None,
                    field_count: None,
                    shape_summary: None,
                    callsite_count: 0,
                    type_use_count: 0,
                });
                if has_body(node) {
                    parent_stack.push(name);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        }
        _ => false,
    };

    if cursor.goto_first_child() {
        loop {
            walk_node(cursor, src, workspace_id, file_path, parent_stack, symbols);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }

    if pushed {
        parent_stack.pop();
    }
}
