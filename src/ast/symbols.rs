use crate::ast::fingerprint::compute_shape_info;
use crate::ast::known_types::extract_type_names;
use crate::types::{PendingTypeUse, Symbol, SymbolKind, Visibility};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Tree};

/// Strip string and character literals from Rust source text.
///
/// Replaces `"..."`, `'...'`, `b"..."`, `r"..."`, `r#"..."#` (any # depth),
/// and `br#"..."#` with empty strings so that `extract_type_names` does not
/// pick up identifiers that only appear inside string data.
fn strip_string_literals(text: &str) -> String {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        // Raw string: r"..." or r#"..."# or br#"..."#
        if i + 1 < len
            && ((bytes[i] == b'r' && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#'))
                || (bytes[i] == b'b'
                    && i + 2 < len
                    && bytes[i + 1] == b'r'
                    && (bytes[i + 2] == b'"' || bytes[i + 2] == b'#')))
        {
            // consume 'b' prefix if present
            if bytes[i] == b'b' {
                i += 1;
            }
            // consume 'r'
            i += 1;
            // count hashes
            let mut hash_count = 0usize;
            while i < len && bytes[i] == b'#' {
                hash_count += 1;
                i += 1;
            }
            // opening "
            if i < len && bytes[i] == b'"' {
                i += 1;
            }
            // skip until closing " followed by hash_count hashes
            while i < len {
                if bytes[i] == b'"' {
                    let mut j = i + 1;
                    let mut hashes = 0usize;
                    while j < len && bytes[j] == b'#' && hashes < hash_count {
                        hashes += 1;
                        j += 1;
                    }
                    if hashes == hash_count {
                        i = j;
                        break;
                    }
                }
                i += 1;
            }
            continue;
        }

        // Byte string: b"..."
        if bytes[i] == b'b' && i + 1 < len && bytes[i + 1] == b'"' {
            i += 2; // skip b"
            while i < len {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        // Normal string: "..."
        if bytes[i] == b'"' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        // Char literal: '...' — only skip if it looks like a char (not a lifetime)
        // A lifetime looks like 'ident; a char literal has at most a few bytes then closing '
        if bytes[i] == b'\'' {
            // Peek ahead: if next byte is a letter that could be a lifetime, check for closing '
            // We consume up to 4 bytes looking for the closing '
            let mut j = i + 1;
            if j < len && bytes[j] == b'\\' {
                j += 2; // escaped char
                if j < len && bytes[j] == b'\'' {
                    i = j + 1;
                    continue;
                }
            } else if j < len && bytes[j] != b'\'' {
                j += 1; // one char
                if j < len && bytes[j] == b'\'' {
                    i = j + 1;
                    continue;
                }
            } else if j < len && bytes[j] == b'\'' {
                // '' empty — just advance past opening '
                i += 1;
                continue;
            }
            // Could not identify as char literal — treat as raw character
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    out
}

/// Extract pending type-use edges from function/method body text.
///
/// Uses the already-normalised body text (comments stripped by `normalize`),
/// then additionally strips string literals before running `extract_type_names`.
/// Returns PendingTypeUse with use_kind="body" attributed to `from_sym`.
fn extract_body_uses(body_text: &str, from_sym: &Symbol) -> Vec<PendingTypeUse> {
    let stripped = strip_string_literals(body_text);
    extract_type_names(&stripped)
        .into_iter()
        .map(|to_name| PendingTypeUse {
            from_name: from_sym.name.clone(),
            from_line: from_sym.line,
            to_name,
            use_kind: "body",
        })
        .collect()
}

/// Extract type names from a `use_declaration` tree-sitter node.
///
/// Handles:
/// - `use crate::foo::Bar;`  → `Bar`
/// - `use crate::foo::{Bar, Baz};`  → `Bar`, `Baz`
/// - `use crate::foo::Bar as Alias;`  → `Bar` (not Alias)
/// - `use crate::foo::*;`  → (skipped)
fn extract_use_decl_names(node: Node<'_>, src: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    collect_use_names(node, src, &mut names);
    names
}

fn collect_use_names(node: Node<'_>, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "use_declaration" => {
            // Recurse into argument child (the use tree)
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_use_names(child, src, out);
            }
        }
        "scoped_identifier" => {
            // e.g. `crate::foo::Bar` — we want the last segment
            // The "name" field is the final identifier
            if let Some(name_node) = node.child_by_field_name("name") {
                let text = name_node.utf8_text(src).unwrap_or("");
                push_if_type_name(text, out);
            }
        }
        "use_as_clause" => {
            // `Bar as Alias` — we want the original name (path child)
            if let Some(path_node) = node.child_by_field_name("path") {
                collect_use_names(path_node, src, out);
            }
        }
        "use_list" => {
            // `{Foo, Bar, Baz}`
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_use_names(child, src, out);
            }
        }
        "identifier" | "type_identifier" => {
            let text = node.utf8_text(src).unwrap_or("");
            push_if_type_name(text, out);
        }
        "scoped_use_list" => {
            // e.g. `crate::foo::{Bar, Baz}`
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_use_names(child, src, out);
            }
        }
        _ => {}
    }
}

fn push_if_type_name(name: &str, out: &mut Vec<String>) {
    if name.is_empty() || name == "self" || name == "super" || name == "crate" {
        return;
    }
    // Only PascalCase names (first char uppercase, len > 1)
    let first = name.as_bytes().first().copied().unwrap_or(0);
    if first.is_ascii_uppercase() && name.len() > 1 {
        let owned = name.to_owned();
        if !out.contains(&owned) {
            out.push(owned);
        }
    }
}

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

/// Extract pending type-use edges from function/method bodies in a tree.
///
/// For each `function_item` or method inside an `impl_item`, retrieves the
/// block body text, normalises it (comments stripped), strips string literals,
/// then runs `extract_type_names` to find PascalCase type references.
/// Attributed to the function Symbol with `use_kind = "body"`.
pub fn extract_body_type_uses(tree: &Tree, src: &[u8], symbols: &[Symbol]) -> Vec<PendingTypeUse> {
    let mut uses = Vec::new();
    let root = tree.root_node();
    collect_body_uses_recursive(root, src, symbols, &mut uses);
    uses
}

fn collect_body_uses_recursive(
    node: tree_sitter::Node<'_>,
    src: &[u8],
    symbols: &[Symbol],
    out: &mut Vec<PendingTypeUse>,
) {
    match node.kind() {
        "function_item" => {
            // Find block child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "block" {
                    let body_text = child.utf8_text(src).unwrap_or("");
                    let norm = normalize(body_text);
                    // Find best matching symbol by start line
                    let fn_line = node.start_position().row as u32 + 1;
                    if let Some(sym) = symbols.iter().find(|s| s.line == fn_line) {
                        let pending = extract_body_uses(&norm, sym);
                        out.extend(pending);
                    }
                    break;
                }
            }
            // Still recurse for nested fns
            let mut cursor2 = node.walk();
            if cursor2.goto_first_child() {
                loop {
                    collect_body_uses_recursive(cursor2.node(), src, symbols, out);
                    if !cursor2.goto_next_sibling() {
                        break;
                    }
                }
                cursor2.goto_parent();
            }
            return;
        }
        _ => {}
    }

    // Default: recurse into children
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_body_uses_recursive(cursor.node(), src, symbols, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// Extract pending type-use edges from `use_declaration` nodes in a tree.
///
/// Attributed to the first Symbol in the file (by ascending line order).
/// If the file has no symbols, returns empty.
/// `use_kind = "use_decl"`.
pub fn extract_use_decl_type_uses(
    tree: &Tree,
    src: &[u8],
    symbols: &[Symbol],
) -> Vec<PendingTypeUse> {
    if symbols.is_empty() {
        return Vec::new();
    }
    // First symbol in file by line (ascending)
    let anchor = symbols
        .iter()
        .min_by_key(|s| s.line)
        .expect("non-empty checked above");

    let mut uses = Vec::new();
    collect_use_decls(tree.root_node(), src, anchor, &mut uses);
    uses
}

fn is_pub_use(node: tree_sitter::Node<'_>, src: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = child.utf8_text(src).unwrap_or("");
            // `pub`, `pub(crate)`, `pub(super)` — all are re-exports
            if text.starts_with("pub") {
                return true;
            }
        }
    }
    false
}

fn collect_use_decls(
    node: tree_sitter::Node<'_>,
    src: &[u8],
    anchor: &Symbol,
    out: &mut Vec<PendingTypeUse>,
) {
    if node.kind() == "use_declaration" {
        let use_kind: &'static str = if is_pub_use(node, src) {
            "pub_use_decl"
        } else {
            "use_decl"
        };
        let names = extract_use_decl_names(node, src);
        for to_name in names {
            out.push(PendingTypeUse {
                from_name: anchor.name.clone(),
                from_line: anchor.line,
                to_name,
                use_kind,
            });
        }
        // Don't recurse further into use_declaration (already handled inside extract_use_decl_names)
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_use_decls(cursor.node(), src, anchor, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
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
