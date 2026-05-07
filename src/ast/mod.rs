pub mod calls;
pub mod fingerprint;
pub mod known_types;
pub mod language;
pub mod languages;
pub mod parser;
pub mod registry;
pub mod symbols;
pub mod synonyms;

pub use calls::extract_calls;
pub use language::LanguageSupport;
pub use parser::RustParser;
pub use registry::{is_supported_extension, language_for_file, supported_extensions};
pub use symbols::extract_symbols;
