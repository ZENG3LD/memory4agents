use crate::ast::{registry, symbols as sym_extractor};
use crate::db::writer;
use crate::error::CoreError;
use crate::graph::crates::parse_multi_manifest_workspace;
use crate::storage::{BatchWriter, DbPool};
use crate::types::{
    CallEdge, FileRecord, IndexRequest, IndexResult, PendingTypeUse, Symbol, SymbolKind,
    TypeUseEdge, Workspace,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Incremental workspace indexer supporting multiple languages.
///
/// Reads go through [`DbPool`] (no contention with any other route); writes
/// are enqueued on [`BatchWriter`] one [`crate::storage::WriteOp`] per file,
/// so a single file's failure rolls back under its own `SAVEPOINT` without
/// losing the rest of the batch. `barrier_async` marks the points where a
/// later phase needs everything before it durably committed — see the
/// per-phase comments below.
pub struct Indexer;

impl Indexer {
    /// Run an incremental index pass for the given request. Blocking work
    /// (filesystem walk, tree-sitter parsing, `cargo metadata`) always runs
    /// inside `spawn_blocking`; this function itself never blocks a tokio
    /// worker thread.
    pub async fn run(
        reader_pool: &DbPool,
        writer_handle: &BatchWriter,
        req: &IndexRequest,
    ) -> Result<IndexResult, CoreError> {
        let start = std::time::Instant::now();
        let root = PathBuf::from(&req.root_path);
        let workspace_id = req.workspace_id.clone();

        // Phase 1: register/upsert the workspace.
        {
            let ws = Workspace {
                id: workspace_id.clone(),
                root_path: req.root_path.clone(),
                name: req.workspace_name.clone(),
                indexed_at: None,
            };
            writer_handle.send(Box::new(move |conn| writer::upsert_workspace(conn, &ws)));
        }

        // Phase 2: collect all supported source files — filesystem walk,
        // no DB involved, run off the async task.
        let all_files = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || collect_source_files(&root)).await?
        };
        let files_scanned = all_files.len() as u32;
        tracing::info!("found {} source files in {:?}", files_scanned, root);

        // Phase 3: filter to files that need (re)indexing.
        let files_to_index: Vec<(PathBuf, String, i64, i64)> = if req.force {
            // Force: wipe existing workspace data first. Must land before
            // any Phase 4 write lands, so barrier both sides of the wipe.
            writer_handle.barrier_async().await;
            let wid = workspace_id.clone();
            writer_handle.send(Box::new(move |conn| {
                conn.execute(
                    "DELETE FROM type_use_edges WHERE workspace_id = ?1",
                    rusqlite::params![wid],
                )?;
                conn.execute("DELETE FROM symbols WHERE workspace_id = ?1", rusqlite::params![wid])?;
                conn.execute("DELETE FROM call_edges WHERE workspace_id = ?1", rusqlite::params![wid])?;
                conn.execute("DELETE FROM file_edges WHERE workspace_id = ?1", rusqlite::params![wid])?;
                conn.execute(
                    "DELETE FROM file_records WHERE workspace_id = ?1",
                    rusqlite::params![wid],
                )?;
                Ok(())
            }));
            writer_handle.barrier_async().await;
            all_files
        } else {
            // Incremental: one read for every stored mtime in this
            // workspace, then filter in-process — no per-file round trip.
            let wid = workspace_id.clone();
            let stored_mtimes: HashMap<String, i64> = reader_pool
                .read(move |conn| {
                    let mut stmt = conn.prepare(
                        "SELECT file_path, mtime FROM file_records WHERE workspace_id = ?1",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![wid], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })?
                        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
                    Ok(rows)
                })
                .await?;

            all_files
                .into_iter()
                .filter(|(_, rel, mtime, _)| stored_mtimes.get(rel) != Some(mtime))
                .collect()
        };

        tracing::info!("{} files need indexing", files_to_index.len());

        // Phase 4: parse (CPU-bound, no DB) then write one WriteOp per file.
        // Each op re-resolves its own name→id map against the transaction
        // the BatchWriter worker happens to flush it in, so it still sees
        // every symbol committed earlier in the same run — including by an
        // earlier file's op in the same flushed batch.
        const CHUNK_SIZE: usize = 50;
        let mut total_symbols: u32 = 0;
        let mut files_indexed: u32 = 0;
        let mut errors: u32 = 0;

        for chunk in files_to_index.chunks(CHUNK_SIZE) {
            let chunk_owned = chunk.to_vec();
            let workspace_id_for_parse = workspace_id.clone();
            let root_path_for_parse = req.root_path.clone();
            let (parsed_batch, parse_errors) = tokio::task::spawn_blocking(move || {
                parse_chunk(&chunk_owned, &workspace_id_for_parse, &root_path_for_parse)
            })
            .await?;
            errors += parse_errors;

            for pf in parsed_batch {
                total_symbols += pf.symbols.len() as u32;
                files_indexed += 1;
                let wid = workspace_id.clone();
                writer_handle.send(Box::new(move |conn| write_one_file(conn, &wid, &pf)));
            }

            tracing::info!("enqueued: {} files indexed so far", files_indexed);
        }

        // Phase 5: remove stale files — must see every Phase 4 write first,
        // and diffs against a fresh disk walk (not the Phase 2 snapshot).
        writer_handle.barrier_async().await;
        {
            let root_for_walk = root.clone();
            let current_files: HashSet<String> = tokio::task::spawn_blocking(move || {
                collect_source_files(&root_for_walk)
                    .into_iter()
                    .map(|(_, rel, _, _)| rel)
                    .collect()
            })
            .await?;
            let wid = workspace_id.clone();
            writer_handle.send(Box::new(move |conn| writer::delete_stale_files(conn, &wid, &current_files)));
        }

        // Phase 6: crate graph — discovers every submodule-rooted
        // `Cargo.toml` one level under `root` (piece 0.7; `nemo` itself has
        // no root manifest), one `cargo metadata` subprocess per manifest,
        // no DB, run via spawn_blocking directly (not through the pool).
        // Stale nodes/edges (e.g. a submodule removed from disk, or the
        // recon's 7 dead rows from a long-gone root manifest) are pruned
        // via replace-all-for-workspace semantics before the upsert loop.
        {
            let root_for_graph = root.clone();
            let wid = workspace_id.clone();
            let graph_result =
                tokio::task::spawn_blocking(move || parse_multi_manifest_workspace(&root_for_graph, &wid)).await?;
            match graph_result {
                Ok((nodes, edges)) => {
                    let current_names: HashSet<String> =
                        nodes.iter().map(|n| n.name.clone()).collect();
                    let current_edges: HashSet<(String, String)> = edges
                        .iter()
                        .map(|e| (e.from_crate.clone(), e.to_crate.clone()))
                        .collect();
                    let wid = workspace_id.clone();
                    writer_handle.send(Box::new(move |conn| {
                        writer::delete_stale_crate_nodes(conn, &wid, &current_names)?;
                        writer::delete_stale_crate_dep_edges(conn, &wid, &current_edges)?;
                        for node in &nodes {
                            if let Err(e) = writer::upsert_crate_node(conn, node) {
                                tracing::warn!("crate node insert error: {e}");
                            }
                        }
                        for edge in &edges {
                            if let Err(e) = writer::upsert_crate_dep_edge(conn, edge) {
                                tracing::warn!("crate dep edge insert error: {e}");
                            }
                        }
                        Ok(())
                    }));
                }
                Err(e) => tracing::warn!("crate graph parse error: {e}"),
            }
        }

        // Phase 7: recompute callsite_count / type_use_count — needs every
        // call_edges/type_use_edges row from Phase 4 committed first.
        writer_handle.barrier_async().await;
        {
            let wid = workspace_id.clone();
            writer_handle.send(Box::new(move |conn| {
                if let Err(e) = writer::update_callsite_counts(conn, &wid) {
                    tracing::warn!("callsite_count update error: {e}");
                }
                if let Err(e) = writer::update_type_use_counts(conn, &wid) {
                    tracing::warn!("type_use_count update error: {e}");
                }
                Ok(())
            }));
        }

        // Phase 8: stamp indexed_at, then barrier so a caller that awaits
        // `run` sees committed data the instant this call returns.
        writer_handle.barrier_async().await;
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
            writer_handle.send(Box::new(move |conn| writer::upsert_workspace(conn, &updated_ws)));
        }
        writer_handle.barrier_async().await;

        let elapsed_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "index complete for workspace={workspace_id}: \
             scanned={files_scanned}, indexed={files_indexed}, \
             symbols={total_symbols}, errors={errors}, elapsed={elapsed_ms}ms"
        );

        Ok(IndexResult {
            workspace_id,
            files_scanned,
            files_indexed,
            symbols_extracted: total_symbols,
            errors,
            elapsed_ms,
        })
    }
}

/// Writes everything extracted from one file inside a single `WriteOp`:
/// delete-stale-for-file, upsert the file record, insert symbols/trigrams,
/// call edges, file edges, then resolve and insert type-use edges. The
/// name→id map is rebuilt from `conn` (the live flush transaction) so it
/// picks up symbols committed by an earlier file's op in the same batch.
fn write_one_file(conn: &rusqlite::Connection, workspace_id: &str, pf: &ParsedFile) -> rusqlite::Result<()> {
    writer::delete_file_symbols(conn, workspace_id, &pf.rel_path)?;

    let file_rec = FileRecord {
        workspace_id: workspace_id.to_string(),
        file_path: pf.rel_path.clone(),
        mtime: pf.mtime,
        size: pf.size,
        symbol_count: pf.symbols.len() as u32,
    };
    writer::upsert_file_record(conn, &file_rec)?;

    // (symbol_start_line, kind_priority, db_id); kind_priority: 2 =
    // function/async_function, 1 = everything else — used to resolve the
    // enclosing symbol for a call/type-use site.
    let mut line_to_id: Vec<(u32, u8, i64)> = Vec::with_capacity(pf.symbols.len());
    for sym in &pf.symbols {
        match writer::insert_symbol(conn, sym) {
            Ok(sym_id) => {
                if let Err(e) = writer::insert_symbol_trigrams(conn, sym_id, &sym.name) {
                    tracing::warn!("trigram insert error: {e}");
                }
                let kind_priority: u8 = match sym.kind {
                    SymbolKind::Function | SymbolKind::AsyncFunction => 2,
                    _ => 1,
                };
                line_to_id.push((sym.line, kind_priority, sym_id));
            }
            Err(e) => tracing::warn!("symbol insert error: {e}"),
        }
    }

    for edge in &pf.calls {
        let mut edge = edge.clone();
        edge.workspace_id = workspace_id.to_string();
        edge.caller_symbol_id = resolve_caller_id(&line_to_id, edge.line);
        if let Err(e) = writer::insert_call_edge(conn, &edge) {
            tracing::warn!("call edge insert error: {e}");
        }
    }

    for edge in &pf.file_edges {
        if let Err(e) = writer::upsert_file_edge(conn, edge) {
            tracing::warn!("file edge insert error: {e}");
        }
    }

    if !pf.pending_type_uses.is_empty() {
        let name_to_id: HashMap<String, i64> = {
            let mut stmt = conn.prepare(
                "SELECT name, MIN(id) FROM symbols WHERE workspace_id = ?1 GROUP BY name",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![workspace_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };

        for pending in &pf.pending_type_uses {
            let from_id = resolve_caller_id(&line_to_id, pending.from_line);
            if from_id == 0 {
                continue;
            }
            if let Some(&to_id) = name_to_id.get(&pending.to_name) {
                if from_id != to_id {
                    let edge = TypeUseEdge {
                        workspace_id: workspace_id.to_string(),
                        from_symbol_id: from_id,
                        to_symbol_id: to_id,
                        use_kind: pending.use_kind,
                    };
                    if let Err(e) = writer::insert_type_use_edge(conn, &edge) {
                        tracing::warn!("type_use_edge insert error: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Parses one chunk of files (tree-sitter, no DB) and returns the parsed
/// results plus a count of read/parse errors. Intended to run inside
/// `spawn_blocking`.
fn parse_chunk(
    chunk: &[(PathBuf, String, i64, i64)],
    workspace_id: &str,
    root_path: &str,
) -> (Vec<ParsedFile>, u32) {
    let mut parsed_batch: Vec<ParsedFile> = Vec::with_capacity(chunk.len());
    let mut errors = 0u32;

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
        let mut pending_type_uses = sym_extractor::extract_type_uses(&symbols);
        if rel_path.ends_with(".rs") {
            pending_type_uses.extend(sym_extractor::extract_body_type_uses(&tree, &src, &symbols));
            pending_type_uses.extend(sym_extractor::extract_use_decl_type_uses(&tree, &src, &symbols));
        }
        let calls = lang.extract_calls(&tree, &src, rel_path, &symbols);
        let file_edges = lang.extract_file_edges(&tree, &src, workspace_id, rel_path, root_path);

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

    (parsed_batch, errors)
}

/// Find the db id of the innermost symbol enclosing `call_line` (1-based).
///
/// `entries` is a list of `(symbol_start_line, kind_priority, db_id)` tuples
/// built during the symbol-insert pass. We pick the entry whose start line is
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

/// Intermediate parsed file data, assembled before the write op.
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

/// `pub(crate)` so `graph::crates::parse_multi_manifest_workspace` (piece
/// 0.7, one-level-deep submodule discovery) and `crate::watch` (piece 0.9,
/// filesystem-event filtering) share this exact same predicate rather than
/// each keeping their own copy of the excluded-name list.
pub(crate) fn is_excluded(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_str().unwrap_or("");
    is_excluded_name(name, entry.file_type().is_dir())
}

/// The name-only half of [`is_excluded`], usable against a plain path
/// component (no `walkdir::DirEntry` available) — `crate::watch` checks
/// every ancestor component of a raw filesystem-event path against this,
/// since (unlike a `WalkDir` traversal that simply never descends into
/// `target/`) a watcher gets one full path per event regardless of depth.
pub(crate) fn is_excluded_name(name: &str, is_dir: bool) -> bool {
    // Skip any hidden directory (starts with '.').
    if name.starts_with('.') && is_dir {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{BatchConfig, Db, DbConfig, MigrationRunner};
    use std::io::Write as _;

    async fn test_writer_and_pool(tag: &str) -> (Db, BatchWriter, DbPool, PathBuf) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "m4a-indexer-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);

        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        db.run_migrations(MigrationRunner::new(crate::storage::migrations::legacy_schema_migrations()))
            .await
            .expect("migration applies");

        let reader_pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("pool opens");
        let writer = BatchWriter::new(db.clone(), BatchConfig::default());
        (db, writer, reader_pool, path)
    }

    fn write_rust_file(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        let mut f = std::fs::File::create(&path).expect("create source file");
        f.write_all(contents.as_bytes()).expect("write source file");
        path
    }

    #[tokio::test]
    async fn incremental_run_only_touches_changed_files() {
        let (db, writer, reader_pool, db_path) = test_writer_and_pool("incremental").await;

        let mut root = std::env::temp_dir();
        root.push(format!(
            "m4a-indexer-src-incremental-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root dir");
        write_rust_file(&root, "a.rs", "pub fn a() {}\n");
        write_rust_file(&root, "b.rs", "pub fn b() {}\n");

        let req = IndexRequest {
            workspace_id: "ws".to_string(),
            root_path: root.to_string_lossy().replace('\\', "/"),
            workspace_name: "ws".to_string(),
            force: false,
        };

        let first = Indexer::run(&reader_pool, &writer, &req).await.expect("first run succeeds");
        assert_eq!(first.files_indexed, 2, "first run must index both new files");

        // Re-running with nothing changed on disk must index zero files.
        let second = Indexer::run(&reader_pool, &writer, &req).await.expect("second run succeeds");
        assert_eq!(second.files_indexed, 0, "unchanged files must not be re-indexed");
        assert_eq!(second.files_scanned, 2);

        drop(writer);
        drop(reader_pool);
        drop(db);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn index_result_counts_match_what_is_queryable_immediately_after_run() {
        let (db, writer, reader_pool, db_path) = test_writer_and_pool("counts").await;

        let mut root = std::env::temp_dir();
        root.push(format!(
            "m4a-indexer-src-counts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root dir");
        write_rust_file(&root, "lib.rs", "pub struct Foo;\npub fn bar() -> Foo { Foo }\n");

        let req = IndexRequest {
            workspace_id: "ws".to_string(),
            root_path: root.to_string_lossy().replace('\\', "/"),
            workspace_name: "ws".to_string(),
            force: false,
        };

        let result = Indexer::run(&reader_pool, &writer, &req).await.expect("run succeeds");
        assert_eq!(result.files_indexed, 1);
        assert!(result.symbols_extracted >= 2, "expected at least the struct and the fn");

        // No barrier here on purpose: IndexResult's own contract (Phase 8's
        // trailing barrier_async) is what must make this immediately visible.
        let wid = "ws".to_string();
        let symbol_count: i64 = reader_pool
            .read(move |conn| conn.query_row("SELECT COUNT(*) FROM symbols WHERE workspace_id = ?1", rusqlite::params![wid], |r| r.get(0)))
            .await
            .expect("read succeeds");
        assert_eq!(symbol_count as u32, result.symbols_extracted);

        drop(writer);
        drop(reader_pool);
        drop(db);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_caller_id_prefers_the_innermost_enclosing_symbol() {
        let entries = vec![(1u32, 1u8, 10i64), (5, 2, 20), (12, 1, 30)];
        assert_eq!(resolve_caller_id(&entries, 7), 20);
        assert_eq!(resolve_caller_id(&entries, 20), 30);
        assert_eq!(resolve_caller_id(&entries, 0), 0);
    }
}
