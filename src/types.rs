use serde::{Deserialize, Serialize};

/// A workspace represents a root directory being indexed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub root_path: String,
    pub name: String,
    pub indexed_at: Option<i64>,
}

/// A symbol extracted from a Rust source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// 0 = placeholder; DB assigns real id via autoincrement.
    pub id: i64,
    pub workspace_id: String,
    pub file_path: String,
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    pub line: u32,
    pub col: u32,
    pub parent_name: Option<String>,
    pub signature: Option<String>,
    pub body_hash: Option<String>,
    pub shape_fingerprint: Option<String>,
    /// Loose shape fingerprint: like shape_fingerprint but ignores field names (struct/enum records).
    pub shape_fingerprint_loose: Option<String>,
    /// Number of fields (struct), variants (enum), or methods (trait).
    pub field_count: Option<u32>,
    /// Human-readable shape summary (real content, truncated to 200 chars).
    pub shape_summary: Option<String>,
    /// Number of times this symbol's name appears as a callee in call_edges (within the same workspace).
    pub callsite_count: u32,
    /// Number of distinct symbols that use this symbol as a type (in-degree from type_use_edges).
    pub type_use_count: u32,
    /// For kind=Impl: the trait name being implemented (e.g. "RenderContext" in `impl RenderContext for Foo`).
    /// None for inherent impls or non-Impl symbols.
    pub impl_trait: Option<String>,
}

/// The kind of a symbol across all supported languages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    // Rust-native
    Function,
    AsyncFunction,
    Struct,
    Enum,
    Trait,
    Impl,
    Const,
    TypeAlias,
    Macro,
    Module,
    // Multi-language
    /// Class definition (Python, TypeScript).
    Class,
    /// Interface declaration (TypeScript, Go).
    Interface,
    /// Method on a class or struct (Python, TypeScript, Go).
    Method,
    /// Module-level variable binding (Python, TypeScript, Go).
    Variable,
    /// Class property (TypeScript).
    Property,
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SymbolKind::Function => "function",
            SymbolKind::AsyncFunction => "async_function",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::Const => "const",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Macro => "macro",
            SymbolKind::Module => "module",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Method => "method",
            SymbolKind::Variable => "variable",
            SymbolKind::Property => "property",
        };
        write!(f, "{s}")
    }
}

impl SymbolKind {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "function" => Some(SymbolKind::Function),
            "async_function" => Some(SymbolKind::AsyncFunction),
            "struct" => Some(SymbolKind::Struct),
            "enum" => Some(SymbolKind::Enum),
            "trait" => Some(SymbolKind::Trait),
            "impl" => Some(SymbolKind::Impl),
            "const" => Some(SymbolKind::Const),
            "type_alias" => Some(SymbolKind::TypeAlias),
            "macro" => Some(SymbolKind::Macro),
            "module" => Some(SymbolKind::Module),
            "class" => Some(SymbolKind::Class),
            "interface" => Some(SymbolKind::Interface),
            "method" => Some(SymbolKind::Method),
            "variable" => Some(SymbolKind::Variable),
            "property" => Some(SymbolKind::Property),
            _ => None,
        }
    }
}

/// Visibility of a Rust symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Private,
    Crate,
    Super,
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::Crate => "crate",
            Visibility::Super => "super",
        };
        write!(f, "{s}")
    }
}

impl Visibility {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "public" => Some(Visibility::Public),
            "private" => Some(Visibility::Private),
            "crate" => Some(Visibility::Crate),
            "super" => Some(Visibility::Super),
            _ => None,
        }
    }
}

/// A pending unresolved type-use edge (collected during AST extraction).
#[derive(Debug, Clone)]
pub struct PendingTypeUse {
    /// Name of the symbol that uses the type (from_symbol).
    pub from_name: String,
    /// Start line of from_symbol (used to resolve its id from the line_to_id table).
    pub from_line: u32,
    /// Name of the type being used (to_name, may not resolve if external).
    pub to_name: String,
    /// Usage kind: 'field' | 'param' | 'return' | 'generic'.
    pub use_kind: &'static str,
}

/// A resolved type-use edge ready for DB insertion.
#[derive(Debug, Clone)]
pub struct TypeUseEdge {
    pub workspace_id: String,
    pub from_symbol_id: i64,
    pub to_symbol_id: i64,
    pub use_kind: &'static str,
}

/// An edge representing a function/method call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEdge {
    pub workspace_id: String,
    pub caller_file: String,
    /// 0 = placeholder before symbol is resolved.
    pub caller_symbol_id: i64,
    pub callee_name: String,
    pub line: u32,
}

/// An edge representing a file-level dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEdge {
    pub workspace_id: String,
    pub from_file: String,
    pub to_file: String,
    pub kind: FileEdgeKind,
}

/// The kind of a file-level edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileEdgeKind {
    UseDecl,
    ModDecl,
}

impl std::fmt::Display for FileEdgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FileEdgeKind::UseDecl => "use_decl",
            FileEdgeKind::ModDecl => "mod_decl",
        };
        write!(f, "{s}")
    }
}

impl FileEdgeKind {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "use_decl" => Some(FileEdgeKind::UseDecl),
            "mod_decl" => Some(FileEdgeKind::ModDecl),
            _ => None,
        }
    }
}

/// A crate in the cargo workspace graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrateNode {
    pub workspace_id: String,
    pub name: String,
    pub version: String,
    pub manifest_path: String,
    /// Relative directory containing the crate's src/ (e.g. "zengeld-terminal/crates/app").
    pub source_dir: String,
    /// True if this package is a direct workspace member.
    pub is_workspace_member: bool,
}

/// A dependency edge between crates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepEdge {
    pub workspace_id: String,
    pub from_crate: String,
    pub to_crate: String,
    pub is_path_dep: bool,
    /// Resolved relative path for path deps, None for registry deps.
    pub dep_path: Option<String>,
}

/// Metadata record for an indexed file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub workspace_id: String,
    pub file_path: String,
    pub mtime: i64,
    pub size: i64,
    pub symbol_count: u32,
}

/// Request to index a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRequest {
    pub workspace_id: String,
    pub root_path: String,
    pub workspace_name: String,
    /// Force re-index even if mtime unchanged.
    pub force: bool,
}

/// Result of an indexing run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResult {
    pub workspace_id: String,
    pub files_scanned: u32,
    pub files_indexed: u32,
    pub symbols_extracted: u32,
    pub errors: u32,
    pub elapsed_ms: u64,
}

/// A search hit from a fuzzy query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub symbol: Symbol,
    pub score: f32,
}

/// Health status of the indexer.
///
/// This is a pure liveness answer: `workspace_count`/`symbol_count`/
/// `file_count` are the LAST KNOWN values from a background cache, not a
/// live database query — see `server::stats`. `stats_age_secs` tells the
/// caller how stale those numbers are; `None` means no refresh has
/// completed yet (e.g. right after server startup). `phase` and
/// `snapshots` (piece 0.12, item 5) are read straight from atomics/an
/// `RwLock`-guarded map — never the database — so this handler answers
/// instantly at any point in the boot sequence; see `server::startup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub ok: bool,
    pub db_path: String,
    pub workspace_count: u32,
    pub symbol_count: u32,
    pub file_count: u32,
    /// Age in seconds of the cached stats above, or `None` if not yet refreshed.
    #[serde(default)]
    pub stats_age_secs: Option<u64>,
    /// Boot-sequence phase — `"migrating"` | `"building_snapshot"` |
    /// `"ready"`. See `server::startup::StartupPhase`.
    #[serde(default = "default_phase")]
    pub phase: String,
    /// Per-workspace read-model snapshot health — empty before any
    /// snapshot has been built.
    #[serde(default)]
    pub snapshots: Vec<SnapshotHealth>,
    /// Reader-pool call/slow-call/deadline-hit counts, aggregated across
    /// every labeled read site since boot. See
    /// `mcp_service4agent::storage::ReadPool::metrics_snapshot` and
    /// `server::state::AppState::reader_deadline_hits`.
    #[serde(default)]
    pub reader_pool: ReaderPoolHealth,
    /// Last known WAL checkpoint outcome, or `None` before the first
    /// background attempt has run. See `server::checkpoint`.
    #[serde(default)]
    pub checkpoint: Option<CheckpointHealth>,
}

fn default_phase() -> String {
    "ready".to_string()
}

/// Aggregate reader-pool health surfaced by `/health` and `mem_health` — see
/// [`HealthStatus::reader_pool`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReaderPoolHealth {
    /// Total labeled reads across every call site since boot.
    pub calls: u64,
    /// Reads that took at least the pool's own slow-query threshold.
    pub slow_calls: u64,
    /// Reads that were interrupted for exceeding their own deadline
    /// (`with_db`'s hot-route path only — see
    /// `server::state::AppState::reader_deadline_hits`).
    pub deadline_hits: u64,
}

/// One WAL checkpoint attempt's outcome, as last recorded by
/// `server::checkpoint`. See [`HealthStatus::checkpoint`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointHealth {
    /// `"passive"` or `"truncate"`.
    pub mode: String,
    pub ok: bool,
    pub wal_size_before_bytes: u64,
    pub wal_size_after_bytes: u64,
    /// Age in seconds of this outcome.
    pub age_secs: u64,
    pub error: Option<String>,
}

/// One workspace's read-model snapshot health, surfaced by `/health` and
/// `mem_health` (piece 0.12, item 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotHealth {
    pub workspace_id: String,
    pub symbol_count: u32,
    pub file_count: u32,
    pub call_edge_count: u32,
    pub age_secs: u64,
    pub build_ms: u64,
    pub estimated_bytes: u64,
}

/// Aggregate statistics for a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceStat {
    pub workspace_id: String,
    pub file_count: u32,
    pub symbol_count: u32,
    pub crate_count: u32,
}
