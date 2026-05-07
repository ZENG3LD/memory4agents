/// Stop-words filtered from name tokens (too generic to be meaningful).
const TOKEN_STOP_WORDS: &[&str] = &[
    "with", "for", "into", "from", "as", "the", "and", "or", "not", "new", "get", "set",
    "is", "to", "of", "in", "on", "at", "by",
];

/// Split a PascalCase / snake_case identifier into lowercase tokens.
///
/// Rules:
/// - CamelCase boundary: lowercase-to-uppercase transition (`KeyCode` → `["key","code"]`)
/// - snake_case: split on `_`
/// - Digits treated as boundaries (`Route3` → `["route", "3"]`)
/// - Tokens shorter than 2 chars are dropped.
/// - Stop-words are filtered.
/// - Result is deduplicated while preserving order.
///
/// Examples:
/// - `ChromeTheme` → `["chrome", "theme"]`
/// - `KeyCode` → `["key", "code"]`
/// - `AnimatedValue` → `["animated", "value"]`
/// - `my_widget_state` → `["my", "widget", "state"]`
pub fn tokenize_name(name: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = name.chars().collect();
    let len = chars.len();

    for i in 0..len {
        let ch = chars[i];
        if ch == '_' {
            if !current.is_empty() {
                flush_token(&mut current, &mut tokens);
            }
        } else if ch.is_ascii_digit() {
            if !current.is_empty() {
                flush_token(&mut current, &mut tokens);
            }
            // digits as their own token
            current.push(ch);
            flush_token(&mut current, &mut tokens);
        } else if ch.is_ascii_uppercase() {
            // Start of a new CamelCase word: flush previous
            if !current.is_empty() {
                // Peek: if next char is lowercase, this is start of new word
                // E.g. "HTML" stays as one token "html", but "HTMLWidget" splits to ["html","widget"]
                let next_is_lower = i + 1 < len && chars[i + 1].is_ascii_lowercase();
                let prev_was_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
                if next_is_lower && prev_was_upper {
                    // "HTMLWidget" — flush "html" before "W"
                    flush_token(&mut current, &mut tokens);
                } else if !prev_was_upper {
                    // Normal transition from lower to upper
                    flush_token(&mut current, &mut tokens);
                }
            }
            current.push(ch.to_ascii_lowercase());
        } else {
            current.push(ch.to_ascii_lowercase());
        }
    }
    if !current.is_empty() {
        flush_token(&mut current, &mut tokens);
    }

    // Filter stop words and short tokens, deduplicate preserving order
    let mut seen = std::collections::HashSet::new();
    tokens
        .into_iter()
        .filter(|t| t.len() >= 2 && !TOKEN_STOP_WORDS.contains(&t.as_str()))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

fn flush_token(current: &mut String, tokens: &mut Vec<String>) {
    if !current.is_empty() {
        tokens.push(current.clone());
        current.clear();
    }
}

/// Returns true if the identifier is a well-known external type that should be
/// excluded from use-graph edges (std / core / common library types, primitives,
/// traits, and common framework types that pollute the graph without carrying
/// structural meaning).
pub fn is_known_external(name: &str) -> bool {
    matches!(
        name,
        // Std containers
        "Vec" | "String" | "Option" | "Result" | "Box" | "Arc" | "Rc"
        | "HashMap" | "HashSet" | "BTreeMap" | "BTreeSet" | "VecDeque"
        | "LinkedList" | "BinaryHeap"
        // Interior mutability / sync
        | "Cell" | "RefCell" | "Mutex" | "RwLock" | "Cow"
        // Path / OS
        | "Path" | "PathBuf" | "OsString" | "OsStr"
        // Primitives
        | "u8" | "u16" | "u32" | "u64" | "u128" | "usize"
        | "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
        | "f32" | "f64" | "bool" | "char" | "str"
        // Time / range
        | "Instant" | "Duration" | "Range" | "RangeInclusive" | "RangeFull"
        // Trait object / iter markers
        | "Self" | "Output" | "Item"
        | "Iterator" | "IntoIterator" | "ExactSizeIterator"
        | "Iter" | "IterMut" | "IntoIter"
        // Common std traits
        | "Send" | "Sync" | "Sized" | "Copy" | "Clone" | "Debug" | "Default"
        | "PartialEq" | "Eq" | "Hash" | "Ord" | "PartialOrd"
        | "Display" | "Error" | "From" | "Into" | "TryFrom" | "TryInto"
        | "Fn" | "FnOnce" | "FnMut"
        | "AsRef" | "AsMut" | "Deref" | "DerefMut" | "Drop"
        // I/O
        | "Read" | "Write" | "BufRead" | "BufWriter" | "BufReader"
        // Async / futures
        | "Future" | "Pin" | "Poll" | "Waker" | "Context"
        // Common winit / wgpu / egui / other framework types that leak
        | "PhysicalSize" | "PhysicalPosition" | "LogicalSize" | "LogicalPosition"
        | "Color" | "Event" | "WindowEvent" | "DeviceEvent"
        // Serde
        | "Serialize" | "Deserialize" | "Serializer" | "Deserializer"
        // Common numeric / algebra
        | "NonZeroU8" | "NonZeroU16" | "NonZeroU32" | "NonZeroU64" | "NonZeroUsize"
        // Atomics
        | "AtomicBool" | "AtomicU8" | "AtomicU16" | "AtomicU32" | "AtomicU64" | "AtomicUsize"
        | "AtomicI8" | "AtomicI16" | "AtomicI32" | "AtomicI64"
    )
}

/// Extract PascalCase type-like identifiers from a signature string.
///
/// Rules:
/// - Starts with an ASCII uppercase letter.
/// - Followed by alphanumeric or `_` characters.
/// - Not in the known-external filter.
/// - Not a single uppercase letter (generic type params like T, U, V, K, E).
/// - Deduplicated.
pub fn extract_type_names(signature: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let bytes = signature.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        let ch = bytes[i] as char;
        if ch.is_ascii_uppercase() {
            // Collect identifier
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &signature[start..i];

            // Skip single-letter type params (T, U, V, K, E, etc.)
            if ident.len() <= 1 {
                continue;
            }
            // Skip known externals
            if is_known_external(ident) {
                continue;
            }
            // Check the character before start — must not be alphanumeric or _ (i.e. we're at a word boundary)
            if start > 0 {
                let prev = bytes[start - 1] as char;
                if prev.is_alphanumeric() || prev == '_' {
                    continue;
                }
            }
            if seen.insert(ident.to_owned()) {
                result.push(ident.to_owned());
            }
        } else {
            i += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_basic() {
        let sig = "pub fn foo(bar: ChromeTheme, baz: Option<Color>) -> Result<Widget, Error>";
        let names = extract_type_names(sig);
        assert!(names.contains(&"ChromeTheme".to_owned()));
        assert!(names.contains(&"Widget".to_owned()));
        // Option, Result, Color, Error should be filtered
        assert!(!names.contains(&"Option".to_owned()));
        assert!(!names.contains(&"Result".to_owned()));
        assert!(!names.contains(&"Error".to_owned()));
    }
}
