use sha2::{Digest, Sha256};
use tree_sitter::Node;

use crate::types::SymbolKind;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Full shape information for a struct/enum/trait/type_alias node.
pub struct ShapeInfo {
    /// Strict fingerprint (includes field names in struct/enum records).
    pub strict: String,
    /// Loose fingerprint (types only, no field names; for traits: no method names).
    pub loose: String,
    /// Number of fields (struct), variants (enum), or methods (trait). 1 for type_alias.
    pub field_count: u32,
    /// Human-readable shape summary, max 200 chars.
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Compute full ShapeInfo. Returns None for non-structural kinds.
pub fn compute_shape_info(node: Node<'_>, src: &[u8], kind: &SymbolKind) -> Option<ShapeInfo> {
    match kind {
        SymbolKind::Struct => struct_shape_info(node, src),
        SymbolKind::Enum => enum_shape_info(node, src),
        SymbolKind::Trait => trait_shape_info(node, src),
        SymbolKind::TypeAlias => type_alias_shape_info(node, src),
        _ => None,
    }
}

/// Backwards-compatible single fingerprint (strict only).
pub fn compute_fingerprint(node: Node<'_>, src: &[u8], kind: &SymbolKind) -> Option<String> {
    compute_shape_info(node, src, kind).map(|info| info.strict)
}

// ---------------------------------------------------------------------------
// Type normalisation
// ---------------------------------------------------------------------------

/// Normalise a type string into a canonical form:
/// - strip `//` line comments and `/* */` block comments
/// - strip lifetimes (`'a`, `'static`, `'_`)
/// - remove leading `&` and `mut ` (reference/mutability decorators)
/// - collapse whitespace
/// - remove spaces around `<>(),:;`
fn normalize_type(s: &str) -> String {
    let no_comments = strip_comments(s);
    let no_lifetimes = strip_lifetimes(&no_comments);
    let no_refs = strip_ref_mut(&no_lifetimes);
    collapse_whitespace_and_punct(&no_refs)
}

fn strip_comments(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Strip lifetime annotations: `'ident` → ``.
fn strip_lifetimes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\'' {
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Remove leading `&` and `mut ` patterns recursively until stable.
fn strip_ref_mut(s: &str) -> String {
    let mut current = s.trim().to_owned();
    loop {
        let t = current.trim();
        if let Some(rest) = t.strip_prefix("&mut ") {
            current = rest.trim().to_owned();
        } else if let Some(rest) = t.strip_prefix("&mut") {
            current = rest.trim().to_owned();
        } else if let Some(rest) = t.strip_prefix('&') {
            current = rest.trim().to_owned();
        } else if let Some(rest) = t.strip_prefix("mut ") {
            current = rest.trim().to_owned();
        } else {
            break;
        }
    }
    current
}

/// Collapse whitespace runs and remove spaces around `<>(),:;`.
fn collapse_whitespace_and_punct(s: &str) -> String {
    let mut collapsed = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
            }
            prev_space = true;
        } else {
            collapsed.push(ch);
            prev_space = false;
        }
    }
    let collapsed = collapsed.trim().to_owned();

    const PUNCT: &[char] = &['<', '>', '(', ')', ',', ':', ';'];
    let mut result = String::with_capacity(collapsed.len());
    let chars: Vec<char> = collapsed.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == ' ' {
            let prev_is_punct = i > 0 && PUNCT.contains(&chars[i - 1]);
            let next_is_punct = i + 1 < chars.len() && PUNCT.contains(&chars[i + 1]);
            if !prev_is_punct && !next_is_punct {
                result.push(' ');
            }
        } else {
            result.push(ch);
        }
        i += 1;
    }
    result.trim().to_owned()
}

// ---------------------------------------------------------------------------
// Node text helper
// ---------------------------------------------------------------------------

fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Summary truncation
// ---------------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

// ---------------------------------------------------------------------------
// Struct shape
// ---------------------------------------------------------------------------

fn struct_shape_info(node: Node<'_>, src: &[u8]) -> Option<ShapeInfo> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "field_declaration_list" => {
                // Named struct
                let mut strict_pairs: Vec<String> = Vec::new();
                let mut types_only: Vec<String> = Vec::new();
                let mut summary_parts: Vec<String> = Vec::new();

                let mut inner = child.walk();
                for field in child.children(&mut inner) {
                    if field.kind() == "field_declaration" {
                        let fname = field
                            .child_by_field_name("name")
                            .map(|n| node_text(n, src))
                            .unwrap_or("")
                            .to_owned();
                        let ftype = field
                            .child_by_field_name("type")
                            .map(|n| normalize_type(node_text(n, src)))
                            .unwrap_or_default();
                        if !fname.is_empty() {
                            strict_pairs.push(format!("{fname}:{ftype}"));
                            types_only.push(ftype.clone());
                            summary_parts.push(format!("{fname}: {ftype}"));
                        }
                    }
                }
                let field_count = strict_pairs.len() as u32;
                strict_pairs.sort();
                let mut types_sorted = types_only.clone();
                types_sorted.sort();

                let strict = sha256_hex(&format!("struct:record:{}", strict_pairs.join(",")));
                let loose = sha256_hex(&format!("loose:struct:record:{}", types_sorted.join(",")));
                let summary = truncate(&format!("{{ {} }}", summary_parts.join(", ")), 200);

                return Some(ShapeInfo { strict, loose, field_count, summary });
            }
            "ordered_field_declaration_list" => {
                // Tuple struct
                let mut types: Vec<String> = Vec::new();
                let mut inner = child.walk();
                for field in child.children(&mut inner) {
                    if let Some(t) = field.child_by_field_name("type") {
                        types.push(normalize_type(node_text(t, src)));
                    } else if field.kind() != "(" && field.kind() != ")" && field.kind() != "," {
                        let txt = node_text(field, src).trim().to_owned();
                        if !txt.is_empty() && !txt.starts_with("pub") {
                            types.push(normalize_type(&txt));
                        }
                    }
                }
                let field_count = types.len() as u32;
                // Strict: sorted (original behaviour)
                let mut sorted = types.clone();
                sorted.sort();
                let strict = sha256_hex(&format!("struct:tuple:{}", sorted.join(",")));
                // Loose: same (no field names to drop for tuple structs)
                let loose = sha256_hex(&format!("loose:struct:tuple:{}", types.join(",")));
                let summary = truncate(&format!("({})", types.join(", ")), 200);

                return Some(ShapeInfo { strict, loose, field_count, summary });
            }
            _ => {}
        }
    }
    // Unit struct
    let strict = sha256_hex("struct:unit");
    let loose = sha256_hex("loose:struct:unit");
    Some(ShapeInfo { strict, loose, field_count: 0, summary: "unit".to_owned() })
}

// ---------------------------------------------------------------------------
// Enum shape
// ---------------------------------------------------------------------------

fn enum_shape_info(node: Node<'_>, src: &[u8]) -> Option<ShapeInfo> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "enum_variant_list" {
            let mut strict_variants: Vec<String> = Vec::new();
            let mut loose_variants: Vec<String> = Vec::new();
            let mut summary_parts: Vec<String> = Vec::new();

            let mut inner = child.walk();
            for variant in child.children(&mut inner) {
                if variant.kind() == "enum_variant" {
                    let vname = variant
                        .child_by_field_name("name")
                        .map(|n| node_text(n, src))
                        .unwrap_or("")
                        .to_owned();
                    if vname.is_empty() {
                        continue;
                    }
                    let (strict_spec, loose_spec, summary_spec) =
                        enum_variant_specs(variant, src, &vname);
                    strict_variants.push(strict_spec);
                    loose_variants.push(loose_spec);
                    summary_parts.push(summary_spec);
                }
            }
            let field_count = strict_variants.len() as u32;
            strict_variants.sort();
            loose_variants.sort();

            let strict = sha256_hex(&format!("enum:{}", strict_variants.join(",")));
            let loose = sha256_hex(&format!("loose:enum:{}", loose_variants.join(",")));
            let summary = truncate(&summary_parts.join(" | "), 200);

            return Some(ShapeInfo { strict, loose, field_count, summary });
        }
    }
    None
}

/// Returns (strict_spec, loose_spec, summary_spec) for one enum variant.
fn enum_variant_specs(variant: Node<'_>, src: &[u8], vname: &str) -> (String, String, String) {
    let mut cursor = variant.walk();
    for child in variant.children(&mut cursor) {
        match child.kind() {
            "field_declaration_list" => {
                // Record variant: { f: T, ... }
                let mut strict_pairs: Vec<String> = Vec::new();
                let mut types_only: Vec<String> = Vec::new();
                let mut sum_parts: Vec<String> = Vec::new();
                let mut inner = child.walk();
                for field in child.children(&mut inner) {
                    if field.kind() == "field_declaration" {
                        let fname = field
                            .child_by_field_name("name")
                            .map(|n| node_text(n, src))
                            .unwrap_or("")
                            .to_owned();
                        let ftype = field
                            .child_by_field_name("type")
                            .map(|n| normalize_type(node_text(n, src)))
                            .unwrap_or_default();
                        if !fname.is_empty() {
                            strict_pairs.push(format!("{fname}:{ftype}"));
                            types_only.push(ftype.clone());
                            sum_parts.push(format!("{fname}: {ftype}"));
                        }
                    }
                }
                strict_pairs.sort();
                let mut types_sorted = types_only;
                types_sorted.sort();
                let strict = format!("{vname}:record:{}", strict_pairs.join(";"));
                // Loose: variant name kept, but field names dropped
                let loose = format!("{vname}|record:{}", types_sorted.join(";"));
                let summary = format!("{vname} {{ {} }}", sum_parts.join(", "));
                return (strict, loose, summary);
            }
            "ordered_field_declaration_list" => {
                // Tuple variant: (T1, T2)
                let mut types: Vec<String> = Vec::new();
                let mut inner = child.walk();
                for field in child.children(&mut inner) {
                    if let Some(t) = field.child_by_field_name("type") {
                        types.push(normalize_type(node_text(t, src)));
                    }
                }
                let strict = format!("{vname}:tuple:{}", types.join(";"));
                let loose = format!("{vname}|tuple:{}", types.join(";"));
                let summary = format!("{vname}({})", types.join(", "));
                return (strict, loose, summary);
            }
            _ => {}
        }
    }
    // Unit variant
    let strict = format!("{vname}:unit");
    let loose = format!("{vname}|unit");
    let summary = vname.to_owned();
    (strict, loose, summary)
}

// ---------------------------------------------------------------------------
// Trait shape
// ---------------------------------------------------------------------------

fn trait_shape_info(node: Node<'_>, src: &[u8]) -> Option<ShapeInfo> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "declaration_list" {
            let mut strict_specs: Vec<String> = Vec::new();
            let mut loose_specs: Vec<String> = Vec::new();
            let mut summary_parts: Vec<String> = Vec::new();

            let mut inner = child.walk();
            for item in child.children(&mut inner) {
                // Both function_signature_item (no body) and function_item (default impl)
                if item.kind() == "function_item" || item.kind() == "function_signature_item" {
                    if let Some((strict, loose, sum)) = method_specs(item, src) {
                        strict_specs.push(strict);
                        loose_specs.push(loose);
                        summary_parts.push(sum);
                    }
                }
            }
            let field_count = strict_specs.len() as u32;
            strict_specs.sort();
            loose_specs.sort();

            let strict = sha256_hex(&format!("trait:{}", strict_specs.join(",")));
            // Loose: no method names (just arity|self_kind|ret_kind per method)
            let loose = sha256_hex(&format!("loose:trait:{}", loose_specs.join(",")));
            let summary = truncate(&summary_parts.join("; "), 200);

            return Some(ShapeInfo { strict, loose, field_count, summary });
        }
    }
    // Trait with no methods
    let strict = sha256_hex("trait:");
    let loose = sha256_hex("loose:trait:");
    Some(ShapeInfo { strict, loose, field_count: 0, summary: String::new() })
}

/// Returns (strict_spec, loose_spec, summary) for one trait method.
/// strict_spec: `name|arity|self_kind|ret_kind`
/// loose_spec:  `arity|self_kind|ret_kind` (no name)
fn method_specs(node: Node<'_>, src: &[u8]) -> Option<(String, String, String)> {
    let name = node.child_by_field_name("name").map(|n| node_text(n, src))?;
    let params_node = node.child_by_field_name("parameters")?;
    let (self_kind, arity) = parse_params(params_node, src);
    let ret_node = node.child_by_field_name("return_type");
    let ret_kind = ret_node
        .map(|n| classify_return_type(node_text(n, src)))
        .unwrap_or("unit");

    let strict = format!("{name}|{arity}|{self_kind}|{ret_kind}");
    let loose = format!("{arity}|{self_kind}|{ret_kind}");
    let summary = if ret_kind == "unit" {
        format!("{name}()")
    } else {
        format!("{name}() -> {ret_kind}")
    };
    Some((strict, loose, summary))
}

fn parse_params(params_node: Node<'_>, src: &[u8]) -> (&'static str, usize) {
    let mut self_kind = "none";
    let mut arity = 0usize;
    let mut cursor = params_node.walk();
    for param in params_node.children(&mut cursor) {
        match param.kind() {
            "self_parameter" | "receiver" => {
                let txt = node_text(param, src);
                if txt.contains("mut") {
                    self_kind = "mut";
                } else if txt.contains('&') {
                    self_kind = "ref";
                } else {
                    self_kind = "owned";
                }
            }
            "parameter" => {
                arity += 1;
            }
            _ => {}
        }
    }
    (self_kind, arity)
}

fn classify_return_type(s: &str) -> &'static str {
    let t = s.trim_start_matches("->").trim();
    if t.starts_with("Result<") {
        "result"
    } else if t.starts_with("Option<") {
        "option"
    } else if t == "bool" {
        "bool"
    } else if t.starts_with("Vec<") {
        "vec"
    } else if t == "String" || t == "&str" || t.starts_with("&'") {
        "string"
    } else if matches!(
        t,
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "usize" | "isize"
    ) {
        "int"
    } else if t == "f32" || t == "f64" {
        "float"
    } else if t.is_empty() || t == "()" {
        "unit"
    } else {
        "other"
    }
}

// ---------------------------------------------------------------------------
// Type alias shape
// ---------------------------------------------------------------------------

fn type_alias_shape_info(node: Node<'_>, src: &[u8]) -> Option<ShapeInfo> {
    let rhs = if let Some(t) = node.child_by_field_name("type") {
        normalize_type(node_text(t, src))
    } else {
        // Fallback: find child after "="
        let mut cursor = node.walk();
        let mut after_eq = false;
        let mut found = String::new();
        for child in node.children(&mut cursor) {
            if child.kind() == "=" {
                after_eq = true;
                continue;
            }
            if after_eq && child.kind() != ";" {
                found = normalize_type(node_text(child, src));
                break;
            }
        }
        if found.is_empty() {
            return None;
        }
        found
    };

    let fp = sha256_hex(&format!("alias:{rhs}"));
    let summary = truncate(&format!("= {rhs}"), 200);
    Some(ShapeInfo {
        strict: fp.clone(),
        loose: fp,
        field_count: 1,
        summary,
    })
}

// ---------------------------------------------------------------------------
// Hash
// ---------------------------------------------------------------------------

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}
