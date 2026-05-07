use crate::ast::{registry, symbols as sym_extractor};
use crate::db::{reader, writer};
use crate::error::CoreError;
use crate::graph::crates::parse_cargo_workspace;
use crate::types::{CallEdge, FileRecord, IndexRequest, IndexResult, PendingTypeUse, Symbol, SymbolKind, TypeUseEdge, Workspace};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Incremental workspace indexer supporting multiple languages.
///
/// Designed for use inside `spawn_blocking`. Holds an `Arc<Mutex<Connection>>`
/// and releases the lock between batches so other requests can proceed.
pub struct Indexer {}

impl Indexer {
    /// Create a new indexer.
    pub fn new() -> Result<Self, CoreError> {
        Ok(Self {})
    }

    /// Run an incremental index pass for the given request.
    ///
    /// The DB mutex is locked only briefly per batch — never for the full walk/parse duration.
    pub fn run(
        &mut self,
        db: &Mutex<rusqlite::Connection>,
        req: &IndexRequest,
    ) -> Result<IndexResult, CoreError> {
        let start = std::time::Instant::now();
        let root = Path::new(&req.root_path);
        let workspace_id = &req.workspace_id;

        // Phase 1: Register/upsert workspace (brief lock).
        {
            let workspace = Workspace {
                id: workspace_id.clone(),
                root_path: req.root_path.clone(),
                name: req.workspace_name.clone(),
                indexed_at: None,
            };
            let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
            writer::upsert_workspace(&conn, &workspace)?;
        }

        // Phase 2: Collect all supported source files — no DB lock needed.
        let all_files = collect_source_files(root);
        let files_scanned = all_files.len() as u32;
        tracing::info!("found {} source files in {:?}", files_scanned, root);

        // Phase 3: Filter to files that need (re)indexing.
        let files_to_index: Vec<(PathBuf, String, i64, i64)> = if req.force {
            // Force: wipe existing workspace data first (brief lock).
            let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
            conn.execute(
                "DELETE FROM type_use_edges WHERE workspace_id = ?1",
                rusqlite::params![workspace_id],
            )?;
            conn.execute(
                "DELETE FROM symbols WHERE workspace_id = ?1",
                rusqlite::params![workspace_id],
            )?;
            conn.execute(
                "DELETE FROM call_edges WHERE workspace_id = ?1",
                rusqlite::params![workspace_id],
            )?;
            conn.execute(
                "DELETE FROM file_edges WHERE workspace_id = ?1",
                rusqlite::params![workspace_id],
            )?;
            conn.execute(
                "DELETE FROM file_records WHERE workspace_id = ?1",
                rusqlite::params![workspace_id],
            )?;
            drop(conn);
            all_files
        } else {
            // Incremental: check mtime for each file (brief lock).
            let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
            let filtered = all_files
                .into_iter()
                .filter(|(_, rel, mtime, _)| {
                    match reader::get_file_mtime(&conn, workspace_id, rel) {
                        Ok(Some(stored)) => stored != *mtime,
                        _ => true,
                    }
                })
                .collect();
            drop(conn);
            filtered
        };

        tracing::info!("{} files need indexing", files_to_index.len());

        // Phase 4: Parse + write in batches of 50.
        const BATCH_SIZE: usize = 50;
        let mut total_symbols: u32 = 0;
        let mut files_indexed: u32 = 0;
        let mut errors: u32 = 0;

        for chunk in files_to_index.chunks(BATCH_SIZE) {
            // Parse all files in this chunk — no DB lock.
            let mut parsed_batch: Vec<ParsedFile> = Vec::with_capacity(chunk.len());

            for (abs_path, rel_path, mtime, size) in chunk {
                let src = match std::fs::read(abs_path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!("read error {}: {}", rel_path, e);
                        errors += 1;
                        continue;
                    }
                };
                let mut lang = match registry::language_for_file(rel_path) {
                    Some(l) => l,
                    None => continue, // unsupported extension
                };
                let tree = match lang.parse(&src) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("parse error {}: {}", rel_path, e);
                        errors += 1;
                        continue;
                    }
                };

                let symbols = lang.extract_symbols(&tree, &src, workspace_id, rel_path);
                let pending_type_uses = sym_extractor::extract_type_uses(&symbols);
                let calls = lang.extract_calls(&tree, &src, rel_path, &symbols);
                let file_edges =
                    lang.extract_file_edges(&tree, &src, workspace_id, rel_path, &req.root_path);

                parsed_batch.push(ParsedFile {
                    rel_path: rel_path.clone(),
                    mtime: *mtime,
                    size: *size,
                    symbols,
                    calls,
                    file_edges,
                    pending_type_uses,
                });
            }

            // Write entire batch inside a single brief transaction.
            // Also collect resolved type-use edges for post-commit insertion.
            let mut batch_type_uses: Vec<TypeUseEdge> = Vec::new();

            {
                let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
                let tx = conn.unchecked_transaction()?;

                // Build a workspace-wide name→first_id HashMap for type-use resolution.
                // This is O(symbols_in_workspace) but done once per batch and takes <1ms.
                let name_to_id: HashMap<String, i64> = {
                    let mut stmt = tx.prepare(
                        "SELECT name, MIN(id) FROM symbols WHERE workspace_id = ?1 GROUP BY name",
                    )?;
                    let collected: HashMap<String, i64> = stmt
                        .query_map(rusqlite::params![workspace_id], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })?
                        .filter_map(|r| r.ok())
                        .collect();
                    collected
                };

                for pf in &parsed_batch {
                    // Delete stale data for this file.
                    writer::delete_file_symbols(&tx, workspace_id, &pf.rel_path)?;

                    let sym_count = pf.symbols.len() as u32;

                    // Upsert file record FIRST so the FK constraint on symbols is satisfied.
                    let file_rec = FileRecord {
                        workspace_id: workspace_id.clone(),
                        file_path: pf.rel_path.clone(),
                        mtime: pf.mtime,
                        size: pf.size,
                        symbol_count: sym_count,
                    };
                    writer::upsert_file_record(&tx, &file_rec)?;

                    // Insert symbols and build a line -> (db_id, kind_priority) map so we
                    // can resolve caller_symbol_id for call edges below.
                    //
                    // Entries: (symbol_start_line, kind_priority, db_id)
                    // kind_priority: 2 = function/async_function, 1 = everything else.
                    let mut line_to_id: Vec<(u32, u8, i64)> = Vec::with_capacity(pf.symbols.len());

                    for sym in &pf.symbols {
                        match writer::insert_symbol(&tx, sym) {
                            Ok(sym_id) => {
                                if let Err(e) =
                                    writer::insert_symbol_trigrams(&tx, sym_id, &sym.name)
                                {
                                    tracing::warn!("trigram insert error: {}", e);
                                }
                                let kind_priority: u8 = match sym.kind {
                                    SymbolKind::Function | SymbolKind::AsyncFunction => 2,
                                    _ => 1,
                                };
                                line_to_id.push((sym.line, kind_priority, sym_id));
                            }
                            Err(e) => {
                                tracing::warn!("symbol insert error: {}", e);
                            }
                        }
                    }

                    // Insert call edges, resolving caller_symbol_id from line_to_id.
                    for edge in &pf.calls {
                        let mut edge: CallEdge = edge.clone();
                        edge.workspace_id = workspace_id.clone();
                        // Resolve the enclosing symbol using DB-assigned ids.
                        edge.caller_symbol_id = resolve_caller_id(&line_to_id, edge.line);
                        if let Err(e) = writer::insert_call_edge(&tx, &edge) {
                            tracing::warn!("call edge insert error: {}", e);
                        }
                    }

                    // Insert file edges.
                    for edge in &pf.file_edges {
                        if let Err(e) = writer::upsert_file_edge(&tx, edge) {
                            tracing::warn!("file edge insert error: {}", e);
                        }
                    }

                    // Resolve type-use edges using the name→id map.
                    for pending in &pf.pending_type_uses {
                        let from_id = resolve_caller_id(&line_to_id, pending.from_line);
                        if from_id == 0 {
                            continue; // Could not resolve from_symbol_id.
                        }
                        if let Some(&to_id) = name_to_id.get(&pending.to_name) {
                            if from_id != to_id {
                                batch_type_uses.push(TypeUseEdge {
                                    workspace_id: workspace_id.clone(),
                                    from_symbol_id: from_id,
                                    to_symbol_id: to_id,
                                    use_kind: pending.use_kind,
                                });
                            }
                        }
                    }

                    total_symbols += sym_count;
                    files_indexed += 1;
                }

                tx.commit()?;
            } // mutex released here

            // Insert resolved type-use edges (separate brief transaction).
            if !batch_type_uses.is_empty() {
                let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
                let tx = conn.unchecked_transaction()?;
                for edge in &batch_type_uses {
                    if let Err(e) = writer::insert_type_use_edge(&tx, edge) {
                        tracing::warn!("type_use_edge insert error: {}", e);
                    }
                }
                tx.commit()?;
            }

            tracing::info!(
                "batch written: {} files indexed so far",
                files_indexed
            );
        }

        // Phase 5: Remove stale files (brief lock).
        {
            let current_files: HashSet<String> = collect_source_files(root)
                .into_iter()
                .map(|(_, rel, _, _)| rel)
                .collect();
            let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
            writer::delete_stale_files(&conn, workspace_id, &current_files)?;
        }

        // Phase 6: Parse cargo workspace for crate graph (brief lock).
        match parse_cargo_workspace(root, workspace_id) {
            Ok((nodes, edges)) => {
                let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
                for node in &nodes {
                    if let Err(e) = writer::upsert_crate_node(&conn, node) {
                        tracing::warn!("crate node insert error: {}", e);
                    }
                }
                for edge in &edges {
                    if let Err(e) = writer::upsert_crate_dep_edge(&conn, edge) {
                        tracing::warn!("crate dep edge insert error: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("crate graph parse error: {}", e);
            }
        }

        // Phase 7: Update callsite_count and type_use_count for all symbols.
        {
            let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
            if let Err(e) = writer::update_callsite_counts(&conn, workspace_id) {
                tracing::warn!("callsite_count update error: {}", e);
            }
            if let Err(e) = writer::update_type_use_counts(&conn, workspace_id) {
                tracing::warn!("type_use_count update error: {}", e);
            }
        }

        // Phase 8: Update workspace indexed_at (brief lock).
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let updated_ws = Workspace {
                id: workspace_id.clone(),
                root_path: req.root_path.clone(),
                name: req.workspace_name.clone(),
                indexed_at: Some(now),
            };
            let conn = db.lock().map_err(|_| CoreError::LockPoisoned)?;
            writer::upsert_workspace(&conn, &updated_ws)?;
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "index complete for workspace={workspace_id}: \
             scanned={files_scanned}, indexed={files_indexed}, \
             symbols={total_symbols}, errors={errors}, elapsed={elapsed_ms}ms"
        );

        Ok(IndexResult {
            workspace_id: workspace_id.clone(),
            files_scanned,
            files_indexed,
            symbols_extracted: total_symbols,
            errors,
            elapsed_ms,
        })
    }
}

/// Find the db id of the innermost symbol enclosing `call_line` (1-based).
///
/// `entries` is a list of `(symbol_start_line, kind_priority, db_id)` tuples
/// built during the symbol-insert pass.  We pick the entry whose start line is
/// the largest value still <= `call_line`, preferring higher `kind_priority`
/// (functions > impls) on ties.
///
/// Returns `0` when no enclosing symbol is found (e.g. top-level calls).
fn resolve_caller_id(entries: &[(u32, u8, i64)], call_line: u32) -> i64 {
    entries
        .iter()
        .filter(|(line, _, _)| *line <= call_line)
        .max_by_key(|(line, priority, _)| (*line, *priority))
        .map(|(_, _, id)| *id)
        .unwrap_or(0)
}

/// Intermediate parsed file data, assembled before the DB lock.
struct ParsedFile {
    rel_path: String,
    mtime: i64,
    size: i64,
    symbols: Vec<Symbol>,
    calls: Vec<CallEdge>,
    file_edges: Vec<crate::types::FileEdge>,
    pending_type_uses: Vec<PendingTypeUse>,
}

/// Collect all supported source files under `root`, returning `(abs_path, rel_path, mtime, size)`.
///
/// The set of supported extensions is determined by the enabled language features.
/// Paths are normalised to forward slashes so they are consistent across platforms.
fn collect_source_files(root: &Path) -> Vec<(PathBuf, String, i64, i64)> {
    let mut result = Vec::new();

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(is_not_excluded)
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("walkdir error: {}", e);
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if !registry::is_supported_extension(ext) {
            continue;
        }

        let meta = match path.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("metadata error for {}: {}", path.display(), e);
                continue;
            }
        };

        let mtime = file_mtime(&meta);
        let size = meta.len() as i64;

        // Normalise to forward slashes for cross-platform consistency.
        let abs_str = path.to_string_lossy().replace('\\', "/");
        let root_str = root.to_string_lossy().replace('\\', "/");
        let rel = if abs_str.starts_with(&root_str) {
            abs_str[root_str.len()..].trim_start_matches('/').to_string()
        } else {
            abs_str.clone()
        };

        result.push((path.to_path_buf(), rel, mtime, size));
    }

    result
}

/// Returns `true` if a WalkDir entry should be skipped entirely.
fn is_not_excluded(entry: &walkdir::DirEntry) -> bool {
    !is_excluded(entry)
}

fn is_excluded(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_str().unwrap_or("");

    // Skip any hidden directory (starts with '.').
    if name.starts_with('.') && entry.file_type().is_dir() {
        return true;
    }

    // Named directories to skip unconditionally.
    matches!(
        name,
        "target" | "node_modules" | ".cargo" | ".git" | "build"
    )
}

fn file_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
