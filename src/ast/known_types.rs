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
