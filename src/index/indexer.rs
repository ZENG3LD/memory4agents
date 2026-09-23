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

/// What triggered an [`Indexer::run`] call and how much of the workspace it
/// should touch.
enum IndexScope {
    /// Full workspace walk — [`IndexRequest::force`] controls whether
    /// existing data is wiped first. Used by `POST /index`.
    Full,
    /// Only these specific paths changed (absolute or already relative to
    /// the workspace root) — used by [`Indexer::run_changed_paths`]
    /// (`crate::watch`'s debounced driver). Skips the full-workspace disk
    /// walk and the workspace-wide mtime-diff read entirely: the caller
    /// already knows exactly what changed, so there is nothing to diff
    /// against. A path that no longer exists on disk has its stored rows
    /// deleted directly (see [`writer::delete_specific_files`]) rather than
    /// via a full stale-file scan. The crate-graph phase only runs if at
    /// least one of these paths is a `Cargo.toml` — `req.force` is ignored
    /// in this scope (a targeted incremental pass never wipes workspace
    /// data).
    Paths(Vec<PathBuf>),
}

/// Incremental workspace indexer supporting multiple languages.
///
/// Reads go through [`DbPool`] (no contention with any other route); writes
/// are enqueued on [`BatchWriter`] one [`crate::storage::WriteOp`] per file,
/// so a single file's failure rolls back under its own `SAVEPOINT` without
/// losing the rest of the batch. `barrier_async` marks the points where a
/// later phase needs everything before it durably committed — see the
/// per-phase comments below.
///
/// Neither `run` nor `run_changed_paths` serializes itself against a
/// concurrent call for the SAME workspace id — that is
/// [`crate::index::lock::IndexLocks`]'s job. Every call site (`POST
/// /index`, `crate::watch`'s driver) holds an `IndexLocks` guard for the
/// full duration of its call, so at most one run per workspace is ever in
/// flight; without that, two concurrent runs' phases could interleave
/// (e.g. one run's force-wipe landing between another run's per-file
/// upserts) even though the `BatchWriter` itself only ever serializes
/// individual ops, not whole runs.
pub struct Indexer;

impl Indexer {
    /// Run a full-workspace index pass for the given request. Blocking work
    /// (filesystem walk, tree-sitter parsing, `cargo metadata`) always runs
    /// inside `spawn_blocking`; this function itself never blocks a tokio
    /// worker thread.
    pub async fn run(
        reader_pool: &DbPool,
        writer_handle: &BatchWriter,
        req: &IndexRequest,
    ) -> Result<IndexResult, CoreError> {
        Self::run_scoped(reader_pool, writer_handle, req, IndexScope::Full).await
    }

    /// Indexes only `changed_paths` instead of walking the whole workspace
    /// — used by `crate::watch`'s debounced driver so a small edit doesn't
    /// pay for a full ~24k-file disk walk or a `cargo metadata` subprocess
    /// per submodule. See [`IndexScope::Paths`] for the exact semantics.
    pub async fn run_changed_paths(
        reader_pool: &DbPool,
        writer_handle: &BatchWriter,
        req: &IndexRequest,
        changed_paths: Vec<PathBuf>,
    ) -> Result<IndexResult, CoreError> {
        Self::run_scoped(reader_pool, writer_handle, req, IndexScope::Paths(changed_paths)).await
    }

    async fn run_scoped(
        reader_pool: &DbPool,
        writer_handle: &BatchWriter,
        req: &IndexRequest,
        scope: IndexScope,
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

        // Phases 2+3: collect the candidate files for this run and filter
        // to the ones that actually need (re)indexing. `IndexScope::Full`
        // walks the whole workspace and diffs against stored mtimes (or
        // wipes everything first under `force`); `IndexScope::Paths` skips
        // both the walk and the diff — the caller already knows exactly
        // what changed.
        let (files_scanned, files_to_index): (u32, Vec<(PathBuf, String, i64, i64)>) = match &scope
        {
            IndexScope::Full => {
                let all_files = {
                    let root = root.clone();
                    tokio::task::spawn_blocking(move || collect_source_files(&root)).await?
                };
                let files_scanned = all_files.len() as u32;
                tracing::info!("found {} source files in {:?}", files_scanned, root);

                let to_index = if req.force {
                    // Force: wipe existing workspace data first. Must land
                    // before any Phase 4 write lands, so barrier both sides
                    // of the wipe.
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
                (files_scanned, to_index)
            }
            IndexScope::Paths(paths) => {
                // The watcher's own fs events are a strictly better "this
                // changed" signal than a stored-mtime diff (which loses to
                // mtime-resolution races), so every given path that still
                // exists is indexed unconditionally — no DB read needed to
                // decide.
                let root_for_stat = root.clone();
                let paths_owned = paths.clone();
                let candidates = tokio::task::spawn_blocking(move || {
                    stat_candidate_files(&root_for_stat, &paths_owned)
                })
                .await?;
                tracing::info!(
                    "watch-triggered pass: {} changed path(s), {} indexable",
                    paths.len(),
                    candidates.len()
                );
                (paths.len() as u32, candidates)
            }
        };

        tracing::info!("{} files need indexing", files_to_index.len());
        let scan_ms = start.elapsed().as_millis();

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
        let parse_enqueue_ms = start.elapsed().as_millis() - scan_ms;

        // Phase 5: remove stale files — must see every Phase 4 write first.
        // `IndexScope::Full` diffs against a fresh disk walk (not the Phase
        // 2 snapshot); `IndexScope::Paths` only checks the given paths
        // themselves — no full re-walk — and deletes rows for exactly the
        // ones no longer on disk.
        //
        // This barrier is Phase 4's actual durability point: it waits for
        // every one of the ~`files_indexed` per-file `WriteOp`s enqueued
        // above to be committed by `BatchWriter`'s worker thread — folded
        // into the final "index complete" line as `flush4_ms` so a
        // regression in write throughput (vs. parse throughput, already
        // covered by `parse_enqueue_ms` above) shows up on its own.
        let before_flush4 = std::time::Instant::now();
        writer_handle.barrier_async().await;
        let flush4_ms = before_flush4.elapsed().as_millis();

        let before_stale_scan = std::time::Instant::now();
        match &scope {
            IndexScope::Full => {
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
            IndexScope::Paths(paths) => {
                let root_for_check = root.clone();
                let paths_owned = paths.clone();
                let vanished: HashSet<String> = tokio::task::spawn_blocking(move || {
                    paths_owned
                        .iter()
                        .filter_map(|p| {
                            let abs = if p.is_absolute() { p.clone() } else { root_for_check.join(p) };
                            if abs.is_file() {
                                None
                            } else {
                                Some(to_relative(&root_for_check, &abs))
                            }
                        })
                        .collect()
                })
                .await?;
                if !vanished.is_empty() {
                    let wid = workspace_id.clone();
                    writer_handle.send(Box::new(move |conn| writer::delete_specific_files(conn, &wid, &vanished)));
                }
            }
        }
        let stale_scan_ms = before_stale_scan.elapsed().as_millis();

        // Phase 6: crate graph — discovers every submodule-rooted
        // `Cargo.toml` one level under `root` (piece 0.7; `nemo` itself has
        // no root manifest), one `cargo metadata` subprocess per manifest,
        // no DB, run via spawn_blocking directly (not through the pool).
        // Stale nodes/edges (e.g. a submodule removed from disk, or the
        // recon's 7 dead rows from a long-gone root manifest) are pruned
        // via replace-all-for-workspace semantics before the upsert loop.
        // `IndexScope::Paths` skips this entirely unless one of the
        // changed paths is itself a `Cargo.toml` — a single edited `.rs`
        // file never needs a `cargo metadata` subprocess per submodule.
        let run_crate_graph_phase = match &scope {
            IndexScope::Full => true,
            IndexScope::Paths(paths) => paths
                .iter()
                .any(|p| p.file_name().map(|f| f == "Cargo.toml").unwrap_or(false)),
        };
        if !run_crate_graph_phase {
            tracing::debug!("skipping crate-graph phase — no Cargo.toml among changed paths");
        }
        let before_crate_graph = std::time::Instant::now();
        if run_crate_graph_phase {
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
        let crate_graph_ms = before_crate_graph.elapsed().as_millis();

        // Phase 7: recompute callsite_count / type_use_count — needs every
        // call_edges/type_use_edges row from Phase 4 committed first. The
        // barrier right after `send` below (not this one) is what actually
        // waits for these two `UPDATE`s to execute and commit — folded into
        // the final line as `counts_ms`, the correlated-subquery `UPDATE`s'
        // own cost isolated from every other phase's.
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
        let before_counts = std::time::Instant::now();
        writer_handle.barrier_async().await;
        let counts_ms = before_counts.elapsed().as_millis();
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
             symbols={total_symbols}, errors={errors}, elapsed={elapsed_ms}ms \
             (scan={scan_ms}ms parse_enqueue={parse_enqueue_ms}ms flush4={flush4_ms}ms \
             stale_scan={stale_scan_ms}ms crate_graph={crate_graph_ms}ms counts={counts_ms}ms)"
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
///
/// When `root` is a git repository, the file set is exactly its tracked
/// files (`git_ls_files`) — filtered by extension and the shared exclusion
/// names — instead of a raw [`WalkDir`] traversal. A `WalkDir` over a
/// workspace root descends into every untracked directory it finds
/// (scratch dirs, `_deprecated/`, `deploy*/`, nested non-submodule repos,
/// …), which is how a full reindex of `nemo` once found ~219k files
/// against a real tracked set of ~25-30k: `--recurse-submodules` alone
/// already covers every submodule, so one `git` call is the whole
/// workspace's scope. `root` not being a git repository (or the `git`
/// command failing to run at all) falls back to the original `WalkDir`
/// traversal unchanged.
fn collect_source_files(root: &Path) -> Vec<(PathBuf, String, i64, i64)> {
    match git_ls_files(root) {
        Some(tracked) => stat_git_tracked_files(root, tracked),
        None => collect_source_files_via_walkdir(root),
    }
}

/// Runs `git -C root ls-files -z --recurse-submodules` and returns the
/// tracked paths (forward-slash separated, relative to `root`) — `None` if
/// `root` is not (recognized by git as) a repository or the command
/// otherwise fails to run, signalling [`collect_source_files`] to fall
/// back to [`collect_source_files_via_walkdir`].
fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let output = crate::proc::hidden("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--recurse-submodules"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|&b| b == 0)
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| String::from_utf8_lossy(chunk).replace('\\', "/"))
            .collect(),
    )
}

/// True if `root` is a git repository as far as `git` itself is concerned
/// — used by [`crate::watch`] to gate its own tracked-files scope rule on
/// exactly the same condition [`collect_source_files`] uses here (a
/// non-git root keeps the watcher's old "every non-excluded path counts"
/// behaviour, mirroring the `WalkDir` fallback above).
pub(crate) fn is_git_repo(root: &Path) -> bool {
    crate::proc::hidden("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Stats exactly the given git-tracked `rel` paths (used by
/// [`collect_source_files`] instead of a `WalkDir` traversal) and returns
/// `(abs_path, rel_path, mtime, size)` for each one that passes the shared
/// exclusion-name filter, has a supported extension, and can still be
/// stat'd (a file git has recorded but that vanished from disk between the
/// `ls-files` call and now is silently skipped — same "caller learns about
/// it on the next pass" discipline [`stat_candidate_files`] already uses).
fn stat_git_tracked_files(root: &Path, tracked: Vec<String>) -> Vec<(PathBuf, String, i64, i64)> {
    let mut result = Vec::with_capacity(tracked.len());

    for rel in tracked {
        if is_relpath_excluded(&rel) {
            continue;
        }
        let ext = match Path::new(&rel).extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if !registry::is_supported_extension(ext) {
            continue;
        }

        let abs = root.join(&rel);
        let meta = match abs.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("metadata error for {}: {}", abs.display(), e);
                continue;
            }
        };

        let mtime = file_mtime(&meta);
        let size = meta.len() as i64;
        result.push((abs, rel, mtime, size));
    }

    result
}

/// `WalkDir`-based fallback used by [`collect_source_files`] when `root`
/// is not a git repository — the original (pre-git-scope) traversal,
/// unchanged.
fn collect_source_files_via_walkdir(root: &Path) -> Vec<(PathBuf, String, i64, i64)> {
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
        let rel = to_relative(root, path);

        result.push((path.to_path_buf(), rel, mtime, size));
    }

    result
}

/// Stats exactly the given `paths` (used by [`IndexScope::Paths`] instead
/// of a full workspace walk) and returns `(abs_path, rel_path, mtime,
/// size)` for each one that currently exists as a plain file with a
/// supported source extension. `paths` may be absolute (as filesystem
/// watcher events are) or already relative to `root`. Duplicates (the same
/// relative path reached via more than one input entry) are collapsed.
fn stat_candidate_files(root: &Path, paths: &[PathBuf]) -> Vec<(PathBuf, String, i64, i64)> {
    let mut result = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for p in paths {
        let abs = if p.is_absolute() { p.clone() } else { root.join(p) };
        let ext = match abs.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if !registry::is_supported_extension(ext) {
            continue;
        }
        let meta = match abs.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue, // vanished, or not a plain file — handled by Phase 5 instead
        };

        let rel = to_relative(root, &abs);
        if !seen.insert(rel.clone()) {
            continue;
        }
        let mtime = file_mtime(&meta);
        let size = meta.len() as i64;
        result.push((abs, rel, mtime, size));
    }

    result
}

/// Shared relative-path normalisation [`collect_source_files`] and
/// [`stat_candidate_files`] both need — forward slashes, `root` prefix
/// stripped, no leading slash.
///
/// On Windows the prefix match ignores ASCII case: the same file reaches
/// here as `C:\…` from a filesystem event and as `c:/…` from a stored
/// workspace root, and a case-sensitive strip would store the absolute path
/// as a second row for the same file.
///
/// `pub(crate)` so `crate::watch`'s git-scope check can turn a raw
/// filesystem-event path into the same relative form
/// [`stat_git_tracked_files`]'s `git ls-files` output already is in.
pub(crate) fn to_relative(root: &Path, abs: &Path) -> String {
    let abs_str = abs.to_string_lossy().replace('\\', "/");
    let root_str = root.to_string_lossy().replace('\\', "/");
    let n = root_str.len();
    let has_root = abs_str.is_char_boundary(n)
        && abs_str.len() >= n
        && if cfg!(windows) {
            abs_str[..n].eq_ignore_ascii_case(&root_str)
        } else {
            abs_str[..n] == root_str
        };
    if has_root {
        abs_str[n..].trim_start_matches('/').to_string()
    } else {
        abs_str
    }
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

/// [`is_excluded_name`] applied to a whole `/`-separated relative path
/// (git's own path separator, regardless of platform) instead of one
/// `walkdir::DirEntry`/path component — used by [`stat_git_tracked_files`].
/// Every directory component is checked with `is_dir = true` (so a hidden
/// or named-excluded ancestor directory is caught, matching `WalkDir`'s own
/// pruning); the final component (the file itself) is checked with
/// `is_dir = false`, matching `WalkDir`'s per-entry semantics where a
/// hidden FILE is never excluded merely for its name.
pub(crate) fn is_relpath_excluded(rel: &str) -> bool {
    let mut parts = rel.split('/').peekable();
    while let Some(part) = parts.next() {
        let is_dir = parts.peek().is_some();
        if is_excluded_name(part, is_dir) {
            return true;
        }
    }
    false
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

    #[cfg(windows)]
    #[test]
    fn to_relative_ignores_drive_letter_case_on_windows() {
        let root = Path::new("c:/Users/me/nemo");
        let event = Path::new(r"C:\Users\me\nemo\uzor\src\lib.rs");
        assert_eq!(to_relative(root, event), "uzor/src/lib.rs");
    }

    #[test]
    fn to_relative_keeps_paths_outside_the_root_absolute() {
        let root = Path::new("/srv/nemo");
        assert_eq!(to_relative(root, Path::new("/srv/other/a.rs")), "/srv/other/a.rs");
        assert_eq!(to_relative(root, Path::new("/srv/nemo/a.rs")), "a.rs");
    }

    #[test]
    fn is_relpath_excluded_matches_the_shared_exclusion_list() {
        assert!(is_relpath_excluded("target/debug/foo.rs"));
        assert!(is_relpath_excluded(".git/HEAD"));
        assert!(is_relpath_excluded("node_modules/x/index.js"));
        assert!(!is_relpath_excluded("src/lib.rs"));
        // A hidden ancestor DIRECTORY is excluded; a hidden leaf FILE by
        // itself is not — matches `WalkDir`'s own per-entry semantics.
        assert!(is_relpath_excluded(".hidden/lib.rs"));
        assert!(!is_relpath_excluded("src/.env"));
        // "build.rs" (a plain Rust build script filename) must never match
        // the "build" directory-name exclusion.
        assert!(!is_relpath_excluded("build.rs"));
        assert!(is_relpath_excluded("build/output.rs"));
    }

    /// Defect fix: a raw `WalkDir` traversal of a workspace root descends
    /// into every untracked directory it finds (scratch dirs, nested
    /// non-submodule repos, …) — this is what made a full reindex of
    /// `nemo` report ~219k files against a real tracked set of ~25-30k.
    /// When `root` is a git repository, `collect_source_files` must use
    /// exactly its tracked files (filtered by extension/exclusion names)
    /// instead — proved here against a real temp git repo rather than a
    /// mocked git command.
    #[test]
    fn collect_source_files_uses_git_scope_when_root_is_a_repository() {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "m4a-indexer-git-scope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root dir");

        let git = |args: &[&str]| {
            crate::proc::hidden("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git command runs")
        };
        assert!(git(&["init", "-q"]).status.success(), "git init must succeed");
        assert!(is_git_repo(&root), "a freshly init'd repo must be recognized as one");

        write_rust_file(&root, "tracked.rs", "pub fn tracked() {}\n");
        write_rust_file(&root, "untracked_scratch.rs", "pub fn scratch() {}\n");
        write_rust_file(&root, "target/debug/build_output.rs", "pub fn out() {}\n");

        // `git add` is enough to make `git ls-files` see a path — no
        // commit (and no configured identity) needed.
        assert!(
            git(&["add", "tracked.rs", "target/debug/build_output.rs"]).status.success(),
            "git add must succeed"
        );

        let files = collect_source_files(&root);
        let rels: HashSet<String> = files.into_iter().map(|(_, rel, _, _)| rel).collect();

        assert!(rels.contains("tracked.rs"), "a tracked source file must be in scope: {rels:?}");
        assert!(
            !rels.contains("untracked_scratch.rs"),
            "an untracked scratch file must be out of scope under the git rule: {rels:?}"
        );
        assert!(
            !rels.contains("target/debug/build_output.rs"),
            "a tracked file under an excluded directory name must still be filtered: {rels:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn is_git_repo_is_false_for_a_plain_non_git_directory() {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "m4a-indexer-not-a-repo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root dir");

        assert!(!is_git_repo(&root), "a plain directory with no .git must not be a repo");

        let _ = std::fs::remove_dir_all(&root);
    }

    use crate::index::lock::IndexLocks;
    use crate::storage::{BatchConfig, Db, DbConfig, MigrationRunner};
    use std::io::Write as _;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

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

    /// Writes a minimal standalone crate one level under `root` (mirroring
    /// `graph::crates`'s own fixture shape: a submodule-rooted manifest,
    /// not a manifest at `root` itself — `parse_multi_manifest_workspace`
    /// only discovers `Cargo.toml` one level deep).
    fn write_fixture_crate(root: &Path, subdir: &str, name: &str) {
        let crate_dir = root.join(subdir);
        std::fs::create_dir_all(crate_dir.join("src")).expect("create crate src dir");
        std::fs::write(crate_dir.join("src/lib.rs"), b"pub fn marker() {}\n")
            .expect("write crate lib.rs");
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .expect("write crate manifest");
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

    /// Defect fix: a watch-triggered batch containing a single edited `.rs`
    /// file must index exactly that file (not walk the whole workspace)
    /// and must NOT invoke the crate-graph phase, even though a perfectly
    /// valid `Cargo.toml` sits on disk in a sibling directory.
    #[tokio::test]
    async fn run_changed_paths_indexes_only_the_given_file_and_skips_crate_graph_without_a_manifest(
    ) {
        let (db, writer, reader_pool, db_path) =
            test_writer_and_pool("changed-paths-no-manifest").await;

        let mut root = std::env::temp_dir();
        root.push(format!(
            "m4a-indexer-changed-paths-no-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root dir");
        write_fixture_crate(&root, "crate-a", "m4a-fixture-changed-paths-no-manifest");
        // Must never be touched by a targeted run over a different file.
        write_rust_file(&root, "untouched.rs", "pub fn untouched() {}\n");

        let req = IndexRequest {
            workspace_id: "ws".to_string(),
            root_path: root.to_string_lossy().replace('\\', "/"),
            workspace_name: "ws".to_string(),
            force: false,
        };

        let changed = vec![root.join("crate-a").join("src").join("lib.rs")];
        let result = Indexer::run_changed_paths(&reader_pool, &writer, &req, changed)
            .await
            .expect("run_changed_paths succeeds");
        assert_eq!(result.files_indexed, 1, "must index exactly the one changed file");

        let wid = "ws".to_string();
        let mut indexed_paths: Vec<String> = reader_pool
            .read(move |conn| {
                conn.prepare("SELECT file_path FROM file_records WHERE workspace_id = ?1")?
                    .query_map(rusqlite::params![wid], |r| r.get(0))?
                    .collect()
            })
            .await
            .expect("read file_records");
        indexed_paths.sort();
        assert_eq!(
            indexed_paths,
            vec!["crate-a/src/lib.rs".to_string()],
            "untouched.rs must never be indexed by a targeted run over a different file"
        );

        let wid2 = "ws".to_string();
        let crate_node_count: i64 = reader_pool
            .read(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM crate_nodes WHERE workspace_id = ?1",
                    rusqlite::params![wid2],
                    |r| r.get(0),
                )
            })
            .await
            .expect("read crate_nodes count");
        assert_eq!(
            crate_node_count, 0,
            "crate-graph phase must not run without a Cargo.toml among the changed paths"
        );

        drop(writer);
        drop(reader_pool);
        drop(db);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Mirror of the test above: a batch containing a `Cargo.toml` DOES
    /// invoke the crate-graph phase.
    #[tokio::test]
    async fn run_changed_paths_runs_crate_graph_when_a_manifest_is_among_the_changed_paths() {
        let (db, writer, reader_pool, db_path) = test_writer_and_pool("changed-paths-manifest").await;

        let mut root = std::env::temp_dir();
        root.push(format!(
            "m4a-indexer-changed-paths-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root dir");
        write_fixture_crate(&root, "crate-a", "m4a-fixture-changed-paths-manifest");

        let req = IndexRequest {
            workspace_id: "ws".to_string(),
            root_path: root.to_string_lossy().replace('\\', "/"),
            workspace_name: "ws".to_string(),
            force: false,
        };

        let changed = vec![root.join("crate-a").join("Cargo.toml")];
        let result = Indexer::run_changed_paths(&reader_pool, &writer, &req, changed)
            .await
            .expect("run_changed_paths succeeds");
        assert_eq!(result.files_indexed, 0, "Cargo.toml itself is never a parsed source file");

        let wid = "ws".to_string();
        let crate_names: Vec<String> = reader_pool
            .read(move |conn| {
                conn.prepare("SELECT name FROM crate_nodes WHERE workspace_id = ?1")?
                    .query_map(rusqlite::params![wid], |r| r.get(0))?
                    .collect()
            })
            .await
            .expect("read crate_nodes");
        assert!(
            crate_names.contains(&"m4a-fixture-changed-paths-manifest".to_string()),
            "crate-graph phase must run when a Cargo.toml is among the changed paths: {crate_names:?}"
        );

        drop(writer);
        drop(reader_pool);
        drop(db);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End-to-end version of `index::lock`'s primitive test, through real
    /// `Indexer::run` calls for the SAME workspace id — proves the fix's
    /// actual wiring (acquire `IndexLocks` for the WHOLE `Indexer::run`
    /// call, exactly like `POST /index` and the watcher driver do), not
    /// just the bare mutex. Run A is held open past its own `Indexer::run`
    /// completion via a channel (deterministic — no timing race needed to
    /// catch a violation); run B must not even start until A releases.
    #[tokio::test]
    async fn concurrent_runs_for_the_same_workspace_never_interleave_under_the_shared_lock() {
        let (db, writer, reader_pool, db_path) = test_writer_and_pool("single-flight").await;
        let locks = IndexLocks::new();

        let mut root = std::env::temp_dir();
        root.push(format!(
            "m4a-indexer-single-flight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root dir");
        write_rust_file(&root, "a.rs", "pub fn a() {}\n");

        let req = IndexRequest {
            workspace_id: "ws".to_string(),
            root_path: root.to_string_lossy().replace('\\', "/"),
            workspace_name: "ws".to_string(),
            force: false,
        };

        let log: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));

        let (hold_release_tx, hold_release_rx) = tokio::sync::oneshot::channel::<()>();
        let (a_started_tx, a_started_rx) = tokio::sync::oneshot::channel::<()>();
        let log_a = log.clone();
        let locks_a = locks.clone();
        let reader_pool_a = reader_pool.clone();
        let writer_a = writer.clone();
        let req_a = req.clone();
        let task_a = tokio::spawn(async move {
            let _guard = locks_a.acquire(&req_a.workspace_id).await;
            log_a.lock().unwrap_or_else(|p| p.into_inner()).push("A-start");
            let _ = a_started_tx.send(());
            Indexer::run(&reader_pool_a, &writer_a, &req_a).await.expect("run A succeeds");
            // Hold the guard open past completion, deterministically, so
            // run B has every chance to (wrongly) start early.
            let _ = hold_release_rx.await;
            log_a.lock().unwrap_or_else(|p| p.into_inner()).push("A-end");
        });
        a_started_rx.await.expect("run A signals it started");

        let log_b = log.clone();
        let locks_b = locks.clone();
        let reader_pool_b = reader_pool.clone();
        let writer_b = writer.clone();
        let req_b = req.clone();
        let task_b = tokio::spawn(async move {
            let _guard = locks_b.acquire(&req_b.workspace_id).await;
            log_b.lock().unwrap_or_else(|p| p.into_inner()).push("B-start");
            Indexer::run(&reader_pool_b, &writer_b, &req_b).await.expect("run B succeeds");
            log_b.lock().unwrap_or_else(|p| p.into_inner()).push("B-end");
        });

        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let snapshot = log.lock().unwrap_or_else(|p| p.into_inner()).clone();
            assert_eq!(
                snapshot,
                vec!["A-start"],
                "run B must not start before run A releases its lock: {snapshot:?}"
            );
        }

        let _ = hold_release_tx.send(());
        task_a.await.expect("run A task must not panic");
        task_b.await.expect("run B task must not panic");

        let final_log = log.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(
            final_log,
            vec!["A-start", "A-end", "B-start", "B-end"],
            "runs for the same workspace must never interleave: {final_log:?}"
        );

        drop(writer);
        drop(reader_pool);
        drop(db);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&root);
    }
}
