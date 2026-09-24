//! [`Snapshot`] — one workspace's immutable, in-memory copy of the graph
//! `crate::db::reader` otherwise re-derives from SQL on every call. See the
//! module doc comment in `crate::model` for why this exists.
//!
//! Built by [`Snapshot::build`] in ONE sequential read pass per table
//! (ordered `SELECT ... WHERE workspace_id = ?1` scans, no per-row
//! queries) over a single connection — see `crate::model::read_model` for
//! how that connection is obtained and how the result is swapped in.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::Connection;

use crate::db::reader::TypeUser;
use crate::error::CoreError;
use crate::search::trigram::{trigram_score, trigrams};
use crate::types::{CrateNode, DepEdge, FileEdge, FileEdgeKind, SearchHit, Symbol, SymbolKind, Visibility};

/// Cap on the number of callers a single [`Snapshot::callers_of`] call
/// materializes — same limit and same reasoning as `db::reader`'s own
/// `MAX_CALLERS` (a bare, heavily-overloaded name can have well over 100k
/// call sites). `pub` (re-exported from `crate::model`) so
/// `server::routes::{ast,mcp}` can both reuse it — `ast` to cap a
/// multi-workspace merge at the same total, `mcp` to decide whether to add
/// the MCP-side truncation marker.
pub const MAX_CALLERS: usize = 1000;

type NameId = u32;
type FileId = u32;
type SymIdx = u32;

/// Compact encoding of `type_use_edges.use_kind` — a small closed set
/// (`crate::ast::symbols` only ever emits these seven values) stored as one
/// byte per edge instead of a heap-allocated `String`, since a workspace
/// this size can carry a few million such edges. `Other` is a documented
/// escape hatch, not a load-bearing path: a value outside the known set
/// would be a new extraction kind added to `crate::ast::symbols` without
/// updating this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeUseKind {
    Field,
    Param,
    Return,
    Generic,
    Body,
    UseDecl,
    PubUseDecl,
    Other,
}

impl TypeUseKind {
    fn parse(s: &str) -> Self {
        match s {
            "field" => TypeUseKind::Field,
            "param" => TypeUseKind::Param,
            "return" => TypeUseKind::Return,
            "generic" => TypeUseKind::Generic,
            "body" => TypeUseKind::Body,
            "use_decl" => TypeUseKind::UseDecl,
            "pub_use_decl" => TypeUseKind::PubUseDecl,
            other => {
                tracing::warn!("[memory4agent] unknown type_use_edges.use_kind: {other}");
                TypeUseKind::Other
            }
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            TypeUseKind::Field => "field",
            TypeUseKind::Param => "param",
            TypeUseKind::Return => "return",
            TypeUseKind::Generic => "generic",
            TypeUseKind::Body => "body",
            TypeUseKind::UseDecl => "use_decl",
            TypeUseKind::PubUseDecl => "pub_use_decl",
            TypeUseKind::Other => "other",
        }
    }
}

/// One `symbols` row, with `name`/`file_path`/`impl_trait` replaced by
/// interned ids — see [`Snapshot::names`]/[`Snapshot::files`].
struct SymbolRow {
    id: i64,
    file: FileId,
    name: NameId,
    kind: SymbolKind,
    visibility: Visibility,
    line: u32,
    col: u32,
    parent_name: Option<String>,
    signature: Option<String>,
    body_hash: Option<String>,
    shape_fingerprint: Option<String>,
    shape_fingerprint_loose: Option<String>,
    field_count: Option<u32>,
    shape_summary: Option<String>,
    callsite_count: u32,
    type_use_count: u32,
    impl_trait: Option<NameId>,
}

/// One `call_edges` row, callee-name already used as the map key —
/// see [`Snapshot::callers_by_callee`].
struct CallSite {
    /// `Some` when `call_edges.caller_symbol_id` resolved to a known
    /// symbol (the normal case) — precise, id-based caller.
    caller_symbol: Option<SymIdx>,
    /// Always set (`call_edges.caller_file`) — used for the legacy
    /// fallback (`caller_symbol_id = 0`): "the first symbol in this file
    /// by line", exactly mirroring `db::reader::get_callers_of`'s own
    /// window-function branch.
    caller_file: FileId,
}

/// One `type_use_edges` row, keyed by `to_symbol_id` — see
/// [`Snapshot::type_use_in`]. Only the "used by" direction is built: none
/// of the ported routes need the "uses" direction today, and an unused
/// field would fail this crate's `-D warnings` gate; add it back the day a
/// route needs it.
struct TypeUseIn {
    from_symbol: SymIdx,
    use_kind: TypeUseKind,
}

/// Row counts and a rough memory estimate for one workspace's snapshot —
/// surfaced by `/health` and `mem_health` (piece 0.12, item 5).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct SnapshotStats {
    pub symbol_count: u32,
    pub file_count: u32,
    pub call_edge_count: u32,
    pub file_edge_count: u32,
    pub type_use_edge_count: u32,
    pub crate_node_count: u32,
    pub crate_dep_edge_count: u32,
    pub distinct_name_count: u32,
    /// Coarse estimate — fixed per-row overhead plus every variable-length
    /// string's actual byte length. Good enough to answer "is this in the
    /// hundreds of MB the plan expects", not a precise allocator accounting.
    pub estimated_bytes: u64,
    pub build_ms: u64,
}

/// One workspace's immutable, in-memory read model. See the module doc
/// comment for the shape and the "why".
pub struct Snapshot {
    workspace_id: String,

    names: Vec<Arc<str>>,
    name_index: HashMap<Arc<str>, NameId>,
    files: Vec<Arc<str>>,
    file_index: HashMap<Arc<str>, FileId>,

    symbols: Vec<SymbolRow>,
    symbol_by_db_id: HashMap<i64, SymIdx>,

    /// Every symbol named `name`, any kind — `line`-then-`file` order is
    /// NOT guaranteed here (insertion order follows the `ORDER BY
    /// file_path, line` scan that built `symbols`, which is sufficient for
    /// [`Snapshot::by_file`] but incidental here).
    by_name: HashMap<NameId, Vec<SymIdx>>,
    /// Symbols defined in a file, in ascending `line` order (relies on the
    /// `ORDER BY file_path, line` scan in [`Snapshot::build`]).
    by_file: HashMap<FileId, Vec<SymIdx>>,

    callers_by_callee: HashMap<NameId, Vec<CallSite>>,

    file_edges_out: HashMap<FileId, Vec<(FileId, FileEdgeKind)>>,
    file_edges_in: HashMap<FileId, Vec<FileId>>,

    type_use_in: HashMap<SymIdx, Vec<TypeUseIn>>,

    impls_by_trait: HashMap<NameId, Vec<SymIdx>>,

    crate_nodes: Vec<CrateNode>,
    crate_dep_edges: Vec<DepEdge>,

    /// Trigram -> every DISTINCT name containing it (built once over
    /// [`Snapshot::names`], not once per symbol — a name defined 18,000
    /// times costs one entry here, not 18,000).
    trigram_index: HashMap<String, Vec<NameId>>,

    stats: SnapshotStats,
    built_at: Instant,
}

impl Snapshot {
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn stats(&self) -> SnapshotStats {
        self.stats
    }

    pub fn age(&self) -> std::time::Duration {
        self.built_at.elapsed()
    }

    /// An empty snapshot for a workspace that hasn't been indexed (or isn't
    /// loaded) yet. Every query method against this naturally returns an
    /// empty result, matching what the retired SQL functions returned for
    /// an unknown/never-indexed `workspace_id` (a `WHERE workspace_id = ?`
    /// scan matching zero rows) — see `server::state::snapshot`, the only
    /// caller.
    pub fn empty(workspace_id: &str) -> Snapshot {
        Snapshot {
            workspace_id: workspace_id.to_string(),
            names: Vec::new(),
            name_index: HashMap::new(),
            files: Vec::new(),
            file_index: HashMap::new(),
            symbols: Vec::new(),
            symbol_by_db_id: HashMap::new(),
            by_name: HashMap::new(),
            by_file: HashMap::new(),
            callers_by_callee: HashMap::new(),
            file_edges_out: HashMap::new(),
            file_edges_in: HashMap::new(),
            type_use_in: HashMap::new(),
            impls_by_trait: HashMap::new(),
            crate_nodes: Vec::new(),
            crate_dep_edges: Vec::new(),
            trigram_index: HashMap::new(),
            stats: SnapshotStats {
                symbol_count: 0,
                file_count: 0,
                call_edge_count: 0,
                file_edge_count: 0,
                type_use_edge_count: 0,
                crate_node_count: 0,
                crate_dep_edge_count: 0,
                distinct_name_count: 0,
                estimated_bytes: 0,
                build_ms: 0,
            },
            built_at: Instant::now(),
        }
    }

    fn intern_name(names: &mut Vec<Arc<str>>, index: &mut HashMap<Arc<str>, NameId>, s: &str) -> NameId {
        if let Some(&id) = index.get(s) {
            return id;
        }
        let arc: Arc<str> = Arc::from(s);
        let id = names.len() as NameId;
        names.push(arc.clone());
        index.insert(arc, id);
        id
    }

    fn name_str(&self, id: NameId) -> &str {
        &self.names[id as usize]
    }

    fn file_str(&self, id: FileId) -> &str {
        &self.files[id as usize]
    }

    fn to_symbol(&self, idx: SymIdx) -> Symbol {
        let row = &self.symbols[idx as usize];
        Symbol {
            id: row.id,
            workspace_id: self.workspace_id.clone(),
            file_path: self.file_str(row.file).to_string(),
            name: self.name_str(row.name).to_string(),
            kind: row.kind.clone(),
            visibility: row.visibility.clone(),
            line: row.line,
            col: row.col,
            parent_name: row.parent_name.clone(),
            signature: row.signature.clone(),
            body_hash: row.body_hash.clone(),
            shape_fingerprint: row.shape_fingerprint.clone(),
            shape_fingerprint_loose: row.shape_fingerprint_loose.clone(),
            field_count: row.field_count,
            shape_summary: row.shape_summary.clone(),
            callsite_count: row.callsite_count,
            type_use_count: row.type_use_count,
            impl_trait: row.impl_trait.map(|id| self.name_str(id).to_string()),
        }
    }

    /// One sequential read pass per table, ordered scans, no per-row
    /// queries — see the module doc comment. `conn` is expected to be one
    /// reader-pool connection checked out for the whole build (see
    /// `crate::model::read_model`).
    pub fn build(conn: &Connection, workspace_id: &str) -> Result<Snapshot, CoreError> {
        let start = Instant::now();

        let mut names: Vec<Arc<str>> = Vec::new();
        let mut name_index: HashMap<Arc<str>, NameId> = HashMap::new();
        let mut files: Vec<Arc<str>> = Vec::new();
        let mut file_index: HashMap<Arc<str>, FileId> = HashMap::new();

        let mut symbols: Vec<SymbolRow> = Vec::new();
        let mut symbol_by_db_id: HashMap<i64, SymIdx> = HashMap::new();
        let mut by_name: HashMap<NameId, Vec<SymIdx>> = HashMap::new();
        let mut by_file: HashMap<FileId, Vec<SymIdx>> = HashMap::new();
        let mut impls_by_trait: HashMap<NameId, Vec<SymIdx>> = HashMap::new();

        {
            let mut stmt = conn.prepare(
                "SELECT id, file_path, name, kind, visibility, line, col, parent_name, signature, \
                 body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, \
                 callsite_count, type_use_count, impl_trait \
                 FROM symbols WHERE workspace_id = ?1 ORDER BY file_path, line",
            )?;
            let mut rows = stmt.query(rusqlite::params![workspace_id])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let file_path: String = row.get(1)?;
                let name: String = row.get(2)?;
                let kind_str: String = row.get(3)?;
                let vis_str: String = row.get(4)?;
                let line: u32 = row.get(5)?;
                let col: u32 = row.get(6)?;
                let parent_name: Option<String> = row.get(7)?;
                let signature: Option<String> = row.get(8)?;
                let body_hash: Option<String> = row.get(9)?;
                let shape_fingerprint: Option<String> = row.get(10)?;
                let shape_fingerprint_loose: Option<String> = row.get(11)?;
                let field_count: Option<i64> = row.get(12)?;
                let shape_summary: Option<String> = row.get(13)?;
                let callsite_count: i64 = row.get(14)?;
                let type_use_count: i64 = row.get(15)?;
                let impl_trait: Option<String> = row.get(16)?;

                let file_id = Self::intern_name(&mut files, &mut file_index, &file_path);
                let name_id = Self::intern_name(&mut names, &mut name_index, &name);
                let impl_trait_id = impl_trait.as_deref().map(|t| Self::intern_name(&mut names, &mut name_index, t));

                let kind = SymbolKind::from_str(&kind_str).unwrap_or_else(|| {
                    tracing::warn!("[memory4agent] unknown SymbolKind: {kind_str}");
                    SymbolKind::Function
                });
                let visibility = Visibility::from_str(&vis_str).unwrap_or_else(|| {
                    tracing::warn!("[memory4agent] unknown Visibility: {vis_str}");
                    Visibility::Private
                });

                let idx = symbols.len() as SymIdx;
                let is_impl = kind == SymbolKind::Impl;
                symbols.push(SymbolRow {
                    id,
                    file: file_id,
                    name: name_id,
                    kind,
                    visibility,
                    line,
                    col,
                    parent_name,
                    signature,
                    body_hash,
                    shape_fingerprint,
                    shape_fingerprint_loose,
                    field_count: field_count.map(|v| v as u32),
                    shape_summary,
                    callsite_count: callsite_count as u32,
                    type_use_count: type_use_count as u32,
                    impl_trait: impl_trait_id,
                });
                symbol_by_db_id.insert(id, idx);
                by_file.entry(file_id).or_default().push(idx);
                by_name.entry(name_id).or_default().push(idx);
                if is_impl {
                    if let Some(trait_id) = impl_trait_id {
                        impls_by_trait.entry(trait_id).or_default().push(idx);
                    }
                }
            }
        }

        let mut callers_by_callee: HashMap<NameId, Vec<CallSite>> = HashMap::new();
        let mut call_edge_count: u32 = 0;
        {
            let mut stmt = conn.prepare(
                "SELECT callee_name, caller_file, caller_symbol_id FROM call_edges WHERE workspace_id = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![workspace_id])?;
            while let Some(row) = rows.next()? {
                let callee_name: String = row.get(0)?;
                let caller_file: String = row.get(1)?;
                let caller_symbol_id: i64 = row.get(2)?;

                let callee_id = Self::intern_name(&mut names, &mut name_index, &callee_name);
                let caller_file_id = Self::intern_name(&mut files, &mut file_index, &caller_file);
                let caller_symbol = if caller_symbol_id != 0 {
                    symbol_by_db_id.get(&caller_symbol_id).copied()
                } else {
                    None
                };
                callers_by_callee
                    .entry(callee_id)
                    .or_default()
                    .push(CallSite { caller_symbol, caller_file: caller_file_id });
                call_edge_count += 1;
            }
        }

        let mut file_edges_out: HashMap<FileId, Vec<(FileId, FileEdgeKind)>> = HashMap::new();
        let mut file_edges_in: HashMap<FileId, Vec<FileId>> = HashMap::new();
        let mut file_edge_count: u32 = 0;
        {
            let mut stmt =
                conn.prepare("SELECT from_file, to_file, kind FROM file_edges WHERE workspace_id = ?1")?;
            let mut rows = stmt.query(rusqlite::params![workspace_id])?;
            while let Some(row) = rows.next()? {
                let from_file: String = row.get(0)?;
                let to_file: String = row.get(1)?;
                let kind_str: String = row.get(2)?;
                let kind = FileEdgeKind::from_str(&kind_str).unwrap_or_else(|| {
                    tracing::warn!("[memory4agent] unknown FileEdgeKind: {kind_str}");
                    FileEdgeKind::UseDecl
                });
                let from_id = Self::intern_name(&mut files, &mut file_index, &from_file);
                let to_id = Self::intern_name(&mut files, &mut file_index, &to_file);
                file_edges_out.entry(from_id).or_default().push((to_id, kind));
                file_edges_in.entry(to_id).or_default().push(from_id);
                file_edge_count += 1;
            }
        }

        let mut type_use_in: HashMap<SymIdx, Vec<TypeUseIn>> = HashMap::new();
        let mut type_use_edge_count: u32 = 0;
        {
            let mut stmt = conn.prepare(
                "SELECT from_symbol_id, to_symbol_id, use_kind FROM type_use_edges WHERE workspace_id = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![workspace_id])?;
            while let Some(row) = rows.next()? {
                let from_symbol_id: i64 = row.get(0)?;
                let to_symbol_id: i64 = row.get(1)?;
                let use_kind: String = row.get(2)?;
                let (Some(&from_idx), Some(&to_idx)) =
                    (symbol_by_db_id.get(&from_symbol_id), symbol_by_db_id.get(&to_symbol_id))
                else {
                    // A type_use_edges row whose endpoint symbol no longer
                    // exists (e.g. a stale row from a partially-cleaned
                    // reindex) — skip rather than fail the whole snapshot
                    // build, matching every SQL reader function's own
                    // "warn and continue" discipline for a bad row.
                    tracing::warn!(
                        "[memory4agent] type_use_edges row references a missing symbol: {from_symbol_id} -> {to_symbol_id}"
                    );
                    continue;
                };
                type_use_in
                    .entry(to_idx)
                    .or_default()
                    .push(TypeUseIn { from_symbol: from_idx, use_kind: TypeUseKind::parse(&use_kind) });
                type_use_edge_count += 1;
            }
        }

        let mut crate_nodes: Vec<CrateNode> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT name, version, manifest_path, source_dir, is_workspace_member \
                 FROM crate_nodes WHERE workspace_id = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![workspace_id])?;
            while let Some(row) = rows.next()? {
                let is_member: i64 = row.get(4)?;
                crate_nodes.push(CrateNode {
                    workspace_id: workspace_id.to_string(),
                    name: row.get(0)?,
                    version: row.get(1)?,
                    manifest_path: row.get(2)?,
                    source_dir: row.get(3)?,
                    is_workspace_member: is_member != 0,
                });
            }
        }

        let mut crate_dep_edges: Vec<DepEdge> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT from_crate, to_crate, is_path_dep, dep_path FROM crate_dep_edges WHERE workspace_id = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![workspace_id])?;
            while let Some(row) = rows.next()? {
                let is_path: i64 = row.get(2)?;
                crate_dep_edges.push(DepEdge {
                    workspace_id: workspace_id.to_string(),
                    from_crate: row.get(0)?,
                    to_crate: row.get(1)?,
                    is_path_dep: is_path != 0,
                    dep_path: row.get(3)?,
                });
            }
        }

        // Trigram index over DISTINCT names — one pass over `names`, not
        // once per symbol (a name with 18,000 definitions still costs one
        // entry here).
        let mut trigram_index: HashMap<String, Vec<NameId>> = HashMap::new();
        for (id, name) in names.iter().enumerate() {
            for t in trigrams(name) {
                trigram_index.entry(t).or_default().push(id as NameId);
            }
        }

        // Coarse memory estimate — fixed per-row overhead plus every
        // variable-length string's actual byte length. Good enough to
        // answer "is this in the hundreds of MB the plan expects", not a
        // precise allocator accounting. Computed inline (rather than as a
        // free function) since every input already lives in this scope and
        // is only ever needed once, at the end of a build.
        let estimated_bytes: u64 = {
            const HASHMAP_ENTRY_OVERHEAD: usize = 48; // bucket + key + Vec header, rough
            let interned_bytes = |v: &[Arc<str>]| -> usize { v.iter().map(|s| s.len() + 24).sum() };
            // `Arc<str>` fat pointer + refcount block, twice over (the
            // `names`/`files` Vec entry and the `name_index`/`file_index`
            // key), plus one HashMap bucket per distinct name/file.
            let intern_table_overhead = |distinct: usize| -> usize { distinct * (32 + 48) };

            let symbol_rows_bytes = symbols.len() * std::mem::size_of::<SymbolRow>();
            let symbol_strings_bytes: usize = symbols
                .iter()
                .map(|s| {
                    s.parent_name.as_ref().map_or(0, String::len)
                        + s.signature.as_ref().map_or(0, String::len)
                        + s.body_hash.as_ref().map_or(0, String::len)
                        + s.shape_fingerprint.as_ref().map_or(0, String::len)
                        + s.shape_fingerprint_loose.as_ref().map_or(0, String::len)
                        + s.shape_summary.as_ref().map_or(0, String::len)
                })
                .sum();

            let file_edges_out_entries: usize = file_edges_out.values().map(|v| v.len()).sum();
            let file_edges_in_entries: usize = file_edges_in.values().map(|v| v.len()).sum();
            let type_use_in_entries: usize = type_use_in.values().map(|v| v.len()).sum();
            let trigram_entries: usize = trigram_index.values().map(|v| v.len()).sum();

            let total = interned_bytes(&names)
                + interned_bytes(&files)
                + intern_table_overhead(names.len())
                + symbol_rows_bytes
                + symbol_strings_bytes
                + call_edge_count as usize * std::mem::size_of::<CallSite>()
                + file_edges_out_entries * std::mem::size_of::<(FileId, FileEdgeKind)>()
                + file_edges_in_entries * std::mem::size_of::<FileId>()
                + type_use_in_entries * std::mem::size_of::<TypeUseIn>()
                + crate_nodes.len() * 128
                + crate_dep_edges.len() * 96
                + trigram_index.len() * HASHMAP_ENTRY_OVERHEAD
                + trigram_entries * std::mem::size_of::<NameId>();
            total as u64
        };

        let stats = SnapshotStats {
            symbol_count: symbols.len() as u32,
            file_count: files.len() as u32,
            call_edge_count,
            file_edge_count,
            type_use_edge_count,
            crate_node_count: crate_nodes.len() as u32,
            crate_dep_edge_count: crate_dep_edges.len() as u32,
            distinct_name_count: names.len() as u32,
            estimated_bytes,
            build_ms: start.elapsed().as_millis() as u64,
        };

        Ok(Snapshot {
            workspace_id: workspace_id.to_string(),
            names,
            name_index,
            files,
            file_index,
            symbols,
            symbol_by_db_id,
            by_name,
            by_file,
            callers_by_callee,
            file_edges_out,
            file_edges_in,
            type_use_in,
            impls_by_trait,
            crate_nodes,
            crate_dep_edges,
            trigram_index,
            stats,
            built_at: start,
        })
    }

    // -----------------------------------------------------------------
    // Queries — one per ported route. See
    // `docs/memory4agent/plans/2026-09-23-memory4agent-plan.md` piece
    // 0.12, item 4 for the exact route list.
    // -----------------------------------------------------------------

    /// Mirrors `search::trigram::fuzzy_search`, but candidates come from
    /// the DISTINCT-name trigram index instead of a per-symbol
    /// `symbol_trigrams` table scan.
    pub fn fuzzy_search(&self, query: &str, limit: u32, threshold: f32) -> Vec<SearchHit> {
        let q_trigrams = trigrams(query);
        if q_trigrams.is_empty() {
            return Vec::new();
        }
        let mut candidate_names: std::collections::HashSet<NameId> = std::collections::HashSet::new();
        for t in &q_trigrams {
            if let Some(ids) = self.trigram_index.get(t) {
                candidate_names.extend(ids.iter().copied());
            }
        }

        let mut hits: Vec<SearchHit> = Vec::new();
        for name_id in candidate_names {
            let score = trigram_score(query, self.name_str(name_id));
            if score < threshold {
                continue;
            }
            if let Some(members) = self.by_name.get(&name_id) {
                for &idx in members {
                    hits.push(SearchHit { symbol: self.to_symbol(idx), score });
                }
            }
        }

        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit as usize);
        hits
    }

    /// Mirrors `db::reader::get_symbols_in_file` (already `ORDER BY line`
    /// via [`Snapshot::by_file`]'s construction order).
    pub fn symbols_in_file(&self, file_path: &str) -> Vec<Symbol> {
        let Some(&file_id) = self.file_index.get(file_path) else { return Vec::new() };
        self.by_file.get(&file_id).map(|v| v.iter().map(|&idx| self.to_symbol(idx)).collect()).unwrap_or_default()
    }

    /// Mirrors `db::reader::count_symbol_definitions`.
    pub fn count_definitions(&self, name: &str) -> u32 {
        self.name_index.get(name).and_then(|id| self.by_name.get(id)).map(|v| v.len() as u32).unwrap_or(0)
    }

    /// Mirrors `db::reader::get_callers_of`: precise (`caller_symbol_id`
    /// resolved) callers first, then the legacy "first symbol in file by
    /// line" fallback for the rare (~750 rows in the live DB) edges where
    /// no enclosing symbol was resolved — capped at [`MAX_CALLERS`] total,
    /// same as the SQL path's `LIMIT`.
    pub fn callers_of(&self, name: &str) -> Vec<Symbol> {
        let Some(&name_id) = self.name_index.get(name) else { return Vec::new() };
        let Some(sites) = self.callers_by_callee.get(&name_id) else { return Vec::new() };

        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut results: Vec<Symbol> = Vec::new();

        for site in sites {
            if results.len() >= MAX_CALLERS {
                return results;
            }
            if let Some(sym_idx) = site.caller_symbol {
                let sym = &self.symbols[sym_idx as usize];
                if seen.insert(sym.id) {
                    results.push(self.to_symbol(sym_idx));
                }
            }
        }

        // Legacy fallback: cap the NUMBER OF DISTINCT FILES considered to
        // `remaining` before resolving — mirrors the SQL path's own
        // `LIMIT remaining` applied to the ranked-by-file set before the
        // final id-based dedup, including its documented quirk (a file
        // whose "first symbol" collides with an id already seen from the
        // precise branch can make the final count come in under
        // `remaining`, rather than backfilling from beyond the limit).
        let remaining = MAX_CALLERS.saturating_sub(results.len());
        let mut seen_files: std::collections::HashSet<FileId> = std::collections::HashSet::new();
        let mut files_considered = 0usize;
        for site in sites {
            if files_considered >= remaining {
                break;
            }
            if site.caller_symbol.is_some() {
                continue;
            }
            if !seen_files.insert(site.caller_file) {
                continue;
            }
            files_considered += 1;
            if let Some(&first_idx) = self.by_file.get(&site.caller_file).and_then(|v| v.first()) {
                let sym = &self.symbols[first_idx as usize];
                if seen.insert(sym.id) {
                    results.push(self.to_symbol(first_idx));
                }
            }
        }

        results
    }

    /// Mirrors `db::reader::get_deps_of_file`.
    pub fn deps_of_file(&self, file_path: &str) -> Vec<FileEdge> {
        let Some(&file_id) = self.file_index.get(file_path) else { return Vec::new() };
        self.file_edges_out
            .get(&file_id)
            .map(|edges| {
                edges
                    .iter()
                    .map(|(to, kind)| FileEdge {
                        workspace_id: self.workspace_id.clone(),
                        from_file: file_path.to_string(),
                        to_file: self.file_str(*to).to_string(),
                        kind: kind.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Mirrors `db::reader::get_reverse_file_deps` — one hop; BFS lives at
    /// the route layer (`server::routes::graph::impact_analysis_impl`),
    /// unchanged from the SQL version's own BFS shape.
    pub fn reverse_deps_of_file(&self, file_path: &str) -> Vec<String> {
        let Some(&file_id) = self.file_index.get(file_path) else { return Vec::new() };
        self.file_edges_in.get(&file_id).map(|v| v.iter().map(|&id| self.file_str(id).to_string()).collect()).unwrap_or_default()
    }

    pub fn crate_nodes(&self) -> &[CrateNode] {
        &self.crate_nodes
    }

    pub fn crate_dep_edges(&self) -> &[DepEdge] {
        &self.crate_dep_edges
    }

    /// Mirrors `db::reader::get_hotspots`: in-degree of `file_edges` for
    /// `.rs` files, top `limit` by count desc. SQL's own `GROUP BY
    /// to_file ORDER BY cnt DESC` has no explicit tie-break; this sorts by
    /// file name ascending first (a stable pre-sort) so ties come out in a
    /// deterministic order for equivalence testing, matching what SQLite's
    /// covering-index scan (`idx_file_edges_to`, already ordered by
    /// `to_file`) produces in practice before the stable `ORDER BY cnt
    /// DESC` reorders only the count.
    pub fn hotspots(&self, limit: u32) -> Vec<(String, u32)> {
        let mut counts: Vec<(FileId, u32)> = self
            .file_edges_in
            .iter()
            .filter(|(file_id, _)| self.file_str(**file_id).ends_with(".rs"))
            .map(|(file_id, from_files)| (*file_id, from_files.len() as u32))
            .collect();
        counts.sort_by(|a, b| self.file_str(a.0).cmp(self.file_str(b.0)));
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        counts.truncate(limit as usize);
        counts.into_iter().map(|(id, cnt)| (self.file_str(id).to_string(), cnt)).collect()
    }

    /// Mirrors `db::reader::resolve_symbol_to_crate`.
    pub fn resolve_symbol_to_crate(&self, name: &str) -> Vec<(Symbol, CrateNode)> {
        let Some(&name_id) = self.name_index.get(name) else { return Vec::new() };
        let Some(members) = self.by_name.get(&name_id) else { return Vec::new() };

        let mut results = Vec::new();
        for &idx in members {
            let sym = self.to_symbol(idx);
            let file_norm = sym.file_path.replace('\\', "/");
            let file_norm = file_norm.trim_start_matches('/');

            let best_crate = self
                .crate_nodes
                .iter()
                .filter(|cn| {
                    if cn.source_dir.is_empty() {
                        return true;
                    }
                    let dir = cn.source_dir.trim_start_matches('/');
                    file_norm.starts_with(dir) && file_norm[dir.len()..].starts_with('/')
                })
                .max_by_key(|cn| cn.source_dir.len());

            if let Some(cn) = best_crate {
                results.push((sym, cn.clone()));
            }
        }
        results
    }

    /// Mirrors `db::reader::get_symbols_by_name` (used by `/ast/duplicates`
    /// — the `workspace`-scoping is inherent since `Snapshot` is already
    /// per-workspace).
    pub fn symbols_by_name(&self, name: &str, kind: Option<&str>) -> Vec<Symbol> {
        let Some(&name_id) = self.name_index.get(name) else { return Vec::new() };
        let Some(members) = self.by_name.get(&name_id) else { return Vec::new() };
        members
            .iter()
            .map(|&idx| self.to_symbol(idx))
            .filter(|s| kind.is_none_or(|k| s.kind.to_string() == k))
            .collect()
    }

    /// Mirrors `db::reader::get_symbols_by_name_prefix`, `ORDER BY
    /// file_path, line`.
    pub fn symbols_by_name_prefix(&self, name: &str, path_prefix: Option<&str>) -> Vec<Symbol> {
        let mut results = self.symbols_by_name(name, None);
        if let Some(prefix) = path_prefix {
            results.retain(|s| s.file_path.starts_with(prefix));
        }
        results.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        results
    }

    /// Mirrors `db::reader::get_type_users`, `ORDER BY name`.
    pub fn type_users(&self, to_symbol_db_id: i64) -> Vec<TypeUser> {
        let Some(&to_idx) = self.symbol_by_db_id.get(&to_symbol_db_id) else { return Vec::new() };
        let Some(edges) = self.type_use_in.get(&to_idx) else { return Vec::new() };
        let mut users: Vec<TypeUser> = edges
            .iter()
            .map(|e| {
                let sym = &self.symbols[e.from_symbol as usize];
                TypeUser {
                    from_symbol_id: sym.id,
                    name: self.name_str(sym.name).to_string(),
                    kind: sym.kind.to_string(),
                    file_path: self.file_str(sym.file).to_string(),
                    line: sym.line,
                    use_kind: e.use_kind.as_str().to_string(),
                }
            })
            .collect();
        users.sort_by(|a, b| a.name.cmp(&b.name));
        users
    }

    /// Mirrors `db::reader::get_impls_of_trait`, `ORDER BY file_path, line`.
    pub fn impls_of_trait(&self, trait_name: &str, path_prefix: Option<&str>) -> Vec<Symbol> {
        let Some(&trait_id) = self.name_index.get(trait_name) else { return Vec::new() };
        let Some(members) = self.impls_by_trait.get(&trait_id) else { return Vec::new() };
        let mut results: Vec<Symbol> = members.iter().map(|&idx| self.to_symbol(idx)).collect();
        if let Some(prefix) = path_prefix {
            results.retain(|s| s.file_path.starts_with(prefix));
        }
        results.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        results
    }

    /// Mirrors `db::reader::get_inherent_impls_of_type`: `kind = 'impl' AND
    /// impl_trait IS NULL AND name = type_name`, `ORDER BY file_path, line`.
    pub fn inherent_impls_of_type(&self, type_name: &str, path_prefix: Option<&str>) -> Vec<Symbol> {
        let mut results: Vec<Symbol> = self
            .symbols_by_name(type_name, Some("impl"))
            .into_iter()
            .filter(|s| s.impl_trait.is_none())
            .collect();
        if let Some(prefix) = path_prefix {
            results.retain(|s| s.file_path.starts_with(prefix));
        }
        results.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        results
    }

    /// Supports `/ast/inherent_impls`'s per-impl method count: symbols in
    /// the SAME file whose `parent_name == impl_name` and whose kind is a
    /// function/async_function/method — mirrors the inline `SELECT
    /// COUNT(*) FROM symbols WHERE ... parent_name = ?3 AND kind IN
    /// (...)` `server::routes::ast::inherent_impls_of_type_impl` ran
    /// per-impl against SQL.
    pub fn method_count_for_impl(&self, file_path: &str, impl_name: &str) -> u32 {
        let Some(&file_id) = self.file_index.get(file_path) else { return 0 };
        let Some(members) = self.by_file.get(&file_id) else { return 0 };
        members
            .iter()
            .filter(|&&idx| {
                let row = &self.symbols[idx as usize];
                matches!(row.kind, SymbolKind::Function | SymbolKind::AsyncFunction | SymbolKind::Method)
                    && row.parent_name.as_deref() == Some(impl_name)
            })
            .count() as u32
    }
}

