use crate::search::trigram::trigrams;
use crate::types::{CallEdge, CrateNode, DepEdge, FileEdge, FileRecord, Symbol, TypeUseEdge, Workspace};
use rusqlite::{params, Connection};
use std::collections::HashSet;

/// Insert or update a workspace record.
pub fn upsert_workspace(conn: &Connection, workspace: &Workspace) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO workspaces (id, root_path, name, indexed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
             root_path  = excluded.root_path,
             name       = excluded.name,
             indexed_at = excluded.indexed_at",
    )?
    .execute(params![
        workspace.id,
        workspace.root_path,
        workspace.name,
        workspace.indexed_at,
    ])?;
    Ok(())
}

/// Insert or update a file record.
pub fn upsert_file_record(conn: &Connection, rec: &FileRecord) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO file_records (workspace_id, file_path, mtime, size, symbol_count)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(workspace_id, file_path) DO UPDATE SET
             mtime        = excluded.mtime,
             size         = excluded.size,
             symbol_count = excluded.symbol_count",
    )?
    .execute(params![
        rec.workspace_id,
        rec.file_path,
        rec.mtime,
        rec.size,
        rec.symbol_count,
    ])?;
    Ok(())
}

/// Identifiers whose denormalised `symbols.callsite_count` /
/// `symbols.type_use_count` may now be stale after a file's edges were
/// removed — returned by [`delete_file_edges`] (via [`delete_file_symbols`],
/// [`delete_stale_files`], [`delete_specific_files`]) so
/// `index::indexer`'s Phase 7 recompute can be scoped to exactly what this
/// run's changes could have affected instead of the whole workspace.
#[derive(Default, Debug)]
pub struct RemovedFileRefs {
    /// Distinct `call_edges.callee_name` values the file's own (now
    /// deleted) call edges used to reference.
    pub callee_names: HashSet<String>,
    /// Distinct `type_use_edges.to_symbol_id` values the file's own (now
    /// deleted) type-use edges used to reference, on either side (a
    /// removed edge FROM this file's symbol still changes some OTHER
    /// symbol's inbound count).
    pub type_use_to_ids: HashSet<i64>,
}

impl RemovedFileRefs {
    fn merge(&mut self, other: RemovedFileRefs) {
        self.callee_names.extend(other.callee_names);
        self.type_use_to_ids.extend(other.type_use_to_ids);
    }
}

/// Captures (before deleting) the callee names / type-use target ids a
/// file's own `call_edges`/`type_use_edges` rows reference, then deletes
/// those rows — but NOT the file's own `symbols`/`file_records` rows,
/// which is why this is a private helper rather than `delete_file_symbols`
/// itself: [`delete_file_symbols`] (a re-indexed file — the caller
/// re-upserts `file_records` right after) and [`delete_stale_files`] /
/// [`delete_specific_files`] (a file gone from disk entirely — the caller
/// deletes `file_records` too, cascading `symbols`) both need this same
/// edge cleanup first.
///
/// Load-bearing, not cosmetic: unlike `symbols` (which cascades off
/// `file_records` via its own `FOREIGN KEY ... ON DELETE CASCADE` —
/// `storage::migrations::BASE_TABLES_SQL`), neither `call_edges` nor
/// `type_use_edges` carries a foreign key at all. Before this fix,
/// [`delete_stale_files`]/[`delete_specific_files`] relied only on that
/// cascade and never touched `call_edges`/`type_use_edges` for a file
/// removed from disk — every edge FROM that file's own (now-gone) symbols
/// lingered forever, permanently inflating whatever `callee_name` or
/// `to_symbol_id` it used to point at.
fn delete_file_edges(
    conn: &Connection,
    workspace_id: &str,
    file_path: &str,
) -> rusqlite::Result<RemovedFileRefs> {
    let mut removed = RemovedFileRefs::default();

    {
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT callee_name FROM call_edges WHERE workspace_id = ?1 AND caller_file = ?2",
        )?;
        let rows = stmt.query_map(params![workspace_id, file_path], |row| row.get::<_, String>(0))?;
        for name in rows.filter_map(|r| r.ok()) {
            removed.callee_names.insert(name);
        }
    }

    // Collect symbol ids before deleting, so we can remove type_use_edges
    // and capture which OTHER symbols' type_use_count this touches.
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare_cached(
            "SELECT id FROM symbols WHERE workspace_id = ?1 AND file_path = ?2",
        )?;
        let collected: Vec<i64> = stmt
            .query_map(params![workspace_id, file_path], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        collected
    };

    {
        let mut stmt = conn.prepare_cached(
            "SELECT to_symbol_id FROM type_use_edges WHERE workspace_id = ?1 AND (from_symbol_id = ?2 OR to_symbol_id = ?2)",
        )?;
        for id in &ids {
            let rows = stmt.query_map(params![workspace_id, id], |row| row.get::<_, i64>(0))?;
            for to_id in rows.filter_map(|r| r.ok()) {
                removed.type_use_to_ids.insert(to_id);
            }
        }
    }

    // Delete type_use_edges where this file's symbols appear on either side.
    let mut delete_type_use_edges = conn.prepare_cached(
        "DELETE FROM type_use_edges WHERE workspace_id = ?1 AND (from_symbol_id = ?2 OR to_symbol_id = ?2)",
    )?;
    for id in &ids {
        delete_type_use_edges.execute(params![workspace_id, id])?;
    }

    conn.prepare_cached("DELETE FROM call_edges WHERE workspace_id = ?1 AND caller_file = ?2")?
        .execute(params![workspace_id, file_path])?;

    Ok(removed)
}

/// Delete all symbols for a given file (cascade removes trigrams; also cleans call/type edges).
///
/// Every statement here is prepared once per connection lifetime via
/// [`Connection::prepare_cached`] and reused for every call after — see the
/// module doc comment on why this matters for indexing throughput.
pub fn delete_file_symbols(
    conn: &Connection,
    workspace_id: &str,
    file_path: &str,
) -> rusqlite::Result<RemovedFileRefs> {
    let removed = delete_file_edges(conn, workspace_id, file_path)?;
    conn.prepare_cached("DELETE FROM symbols WHERE workspace_id = ?1 AND file_path = ?2")?
        .execute(params![workspace_id, file_path])?;
    Ok(removed)
}

/// Insert a symbol and return its new rowid.
pub fn insert_symbol(conn: &Connection, sym: &Symbol) -> rusqlite::Result<i64> {
    conn.prepare_cached(
        "INSERT INTO symbols (workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
    )?
    .execute(params![
        sym.workspace_id,
        sym.file_path,
        sym.name,
        sym.kind.to_string(),
        sym.visibility.to_string(),
        sym.line,
        sym.col,
        sym.parent_name,
        sym.signature,
        sym.body_hash,
        sym.shape_fingerprint,
        sym.shape_fingerprint_loose,
        sym.field_count.map(|v| v as i64),
        sym.shape_summary,
        sym.callsite_count as i64,
        sym.type_use_count as i64,
        sym.impl_trait,
    ])?;
    Ok(conn.last_insert_rowid())
}

/// Insert a type-use edge (ignore on duplicate primary key).
pub fn insert_type_use_edge(conn: &Connection, edge: &TypeUseEdge) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO type_use_edges (workspace_id, from_symbol_id, to_symbol_id, use_kind)
         VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![
        edge.workspace_id,
        edge.from_symbol_id,
        edge.to_symbol_id,
        edge.use_kind,
    ])?;
    Ok(())
}

/// Update type_use_count for all symbols in a workspace from type_use_edges (in-degree).
///
/// `INDEXED BY idx_type_use_to` is load-bearing, not cosmetic: `type_use_edges`'s
/// own `PRIMARY KEY (workspace_id, from_symbol_id, to_symbol_id, use_kind)`
/// gives SQLite a same-prefix autoindex on `workspace_id` that its planner
/// prefers by default over `idx_type_use_to (workspace_id, to_symbol_id)` —
/// which means the correlated subquery's `to_symbol_id = symbols.id` filter
/// was never actually seeked on, only applied as a residual check after
/// scanning every `type_use_edges` row for the workspace, for every single
/// `symbols` row (`O(symbols x edges)`). Measured on a ~1k-file/~22k-symbol/
/// ~30k-edge sample: ~30s without the hint, ~1-3s with it.
pub fn update_type_use_counts(conn: &Connection, workspace_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE symbols
         SET type_use_count = (
             SELECT COUNT(DISTINCT from_symbol_id)
             FROM type_use_edges INDEXED BY idx_type_use_to
             WHERE type_use_edges.workspace_id = symbols.workspace_id
               AND type_use_edges.to_symbol_id = symbols.id
         )
         WHERE workspace_id = ?1",
        params![workspace_id],
    )?;
    Ok(())
}

/// Update callsite_count for all symbols in a workspace from call_edges.
/// Already seeks via `idx_call_edges_callee (callee_name, workspace_id)` —
/// `call_edges` has a plain `INTEGER PRIMARY KEY AUTOINCREMENT` rowid, so
/// there is no competing same-prefix autoindex for the planner to prefer
/// instead (unlike [`update_type_use_counts`]'s own `type_use_edges`).
pub fn update_callsite_counts(conn: &Connection, workspace_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE symbols
         SET callsite_count = (
             SELECT COUNT(*) FROM call_edges
             WHERE call_edges.workspace_id = symbols.workspace_id
               AND call_edges.callee_name = symbols.name
         )
         WHERE symbols.workspace_id = ?1",
        params![workspace_id],
    )?;
    Ok(())
}

/// Ceiling on how many placeholders [`update_callsite_counts_for_names`]/
/// [`update_type_use_counts_for_symbols`] put in one `IN (...)` clause —
/// chunked regardless of the SQLite build's own variable-count ceiling, and
/// to keep each individual statement's own planning cost bounded even for
/// an unusually large touched set (e.g. a broad rename sweep).
const MAX_IN_CLAUSE: usize = 500;

/// Scoped version of [`update_callsite_counts`]: recomputes `callsite_count`
/// only for symbols named one of `names`, instead of the whole workspace.
/// Used by `index::indexer`'s incremental Phase 7 (only files/symbols this
/// run's changes actually touched) — the whole-workspace version stays in
/// use for a `force=true` full rebuild, where virtually every name is
/// touched anyway and one bulk `UPDATE` beats `names.len()` narrow ones.
/// No-op on an empty `names` (nothing to recompute).
pub fn update_callsite_counts_for_names(
    conn: &Connection,
    workspace_id: &str,
    names: &HashSet<String>,
) -> rusqlite::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let all: Vec<&String> = names.iter().collect();
    for chunk in all.chunks(MAX_IN_CLAUSE) {
        let placeholders: Vec<String> = (2..2 + chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "UPDATE symbols
             SET callsite_count = (
                 SELECT COUNT(*) FROM call_edges
                 WHERE call_edges.workspace_id = symbols.workspace_id
                   AND call_edges.callee_name = symbols.name
             )
             WHERE symbols.workspace_id = ?1 AND symbols.name IN ({})",
            placeholders.join(", ")
        );
        let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(chunk.len() + 1);
        param_values.push(Box::new(workspace_id.to_owned()));
        for name in chunk {
            param_values.push(Box::new((*name).clone()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        conn.execute(&sql, refs.as_slice())?;
    }
    Ok(())
}

/// Scoped version of [`update_type_use_counts`]: recomputes `type_use_count`
/// only for the given symbol ids, instead of the whole workspace. Same
/// `INDEXED BY idx_type_use_to` hint and the same full-rebuild-keeps-the-
/// whole-workspace-version rationale as [`update_callsite_counts_for_names`].
/// No-op on an empty `symbol_ids`.
pub fn update_type_use_counts_for_symbols(
    conn: &Connection,
    workspace_id: &str,
    symbol_ids: &HashSet<i64>,
) -> rusqlite::Result<()> {
    if symbol_ids.is_empty() {
        return Ok(());
    }
    let all: Vec<i64> = symbol_ids.iter().copied().collect();
    for chunk in all.chunks(MAX_IN_CLAUSE) {
        let placeholders: Vec<String> = (2..2 + chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "UPDATE symbols
             SET type_use_count = (
                 SELECT COUNT(DISTINCT from_symbol_id)
                 FROM type_use_edges INDEXED BY idx_type_use_to
                 WHERE type_use_edges.workspace_id = symbols.workspace_id
                   AND type_use_edges.to_symbol_id = symbols.id
             )
             WHERE symbols.workspace_id = ?1 AND symbols.id IN ({})",
            placeholders.join(", ")
        );
        let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(chunk.len() + 1);
        param_values.push(Box::new(workspace_id.to_owned()));
        for id in chunk {
            param_values.push(Box::new(*id));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        conn.execute(&sql, refs.as_slice())?;
    }
    Ok(())
}

/// Compute trigrams for a name and insert them into symbol_trigrams. One
/// name can carry a dozen-plus trigrams, and one file a few dozen symbols —
/// [`Connection::prepare_cached`] means only the FIRST call in the
/// connection's lifetime actually compiles this statement.
pub fn insert_symbol_trigrams(
    conn: &Connection,
    symbol_id: i64,
    name: &str,
) -> rusqlite::Result<()> {
    let mut stmt =
        conn.prepare_cached("INSERT OR IGNORE INTO symbol_trigrams (symbol_id, trigram) VALUES (?1, ?2)")?;
    for tgram in trigrams(name) {
        stmt.execute(params![symbol_id, tgram])?;
    }
    Ok(())
}

/// Insert a call edge.
pub fn insert_call_edge(conn: &Connection, edge: &CallEdge) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO call_edges (workspace_id, caller_file, caller_symbol_id, callee_name, line)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        edge.workspace_id,
        edge.caller_file,
        edge.caller_symbol_id,
        edge.callee_name,
        edge.line,
    ])?;
    Ok(())
}

/// Insert or update a file edge.
pub fn upsert_file_edge(conn: &Connection, edge: &FileEdge) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO file_edges (workspace_id, from_file, to_file, kind)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_id, from_file, to_file, kind) DO NOTHING",
    )?
    .execute(params![
        edge.workspace_id,
        edge.from_file,
        edge.to_file,
        edge.kind.to_string(),
    ])?;
    Ok(())
}

/// Insert or update a crate node.
pub fn upsert_crate_node(conn: &Connection, node: &CrateNode) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO crate_nodes (workspace_id, name, version, manifest_path, source_dir, is_workspace_member)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(workspace_id, name) DO UPDATE SET
             version             = excluded.version,
             manifest_path       = excluded.manifest_path,
             source_dir          = excluded.source_dir,
             is_workspace_member = excluded.is_workspace_member",
        params![
            node.workspace_id,
            node.name,
            node.version,
            node.manifest_path,
            node.source_dir,
            node.is_workspace_member as i32,
        ],
    )?;
    Ok(())
}

/// Insert or update a crate dependency edge.
pub fn upsert_crate_dep_edge(conn: &Connection, edge: &DepEdge) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO crate_dep_edges (workspace_id, from_crate, to_crate, is_path_dep, dep_path)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(workspace_id, from_crate, to_crate) DO UPDATE SET
             is_path_dep = excluded.is_path_dep,
             dep_path    = excluded.dep_path",
        params![
            edge.workspace_id,
            edge.from_crate,
            edge.to_crate,
            edge.is_path_dep as i32,
            edge.dep_path,
        ],
    )?;
    Ok(())
}

/// Delete crate nodes for a workspace whose name is no longer present in a
/// freshly parsed [`crate::graph::crates::parse_multi_manifest_workspace`]
/// result — replace-all-for-workspace semantics, same shape as
/// [`delete_stale_files`] below. This is what makes a crate removed from
/// disk (or the recon's 7 dead rows from a long-gone root manifest)
/// actually disappear on the next `/index`, instead of accumulating forever
/// under upsert-only semantics.
pub fn delete_stale_crate_nodes(
    conn: &Connection,
    workspace_id: &str,
    current_names: &HashSet<String>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("SELECT name FROM crate_nodes WHERE workspace_id = ?1")?;
    let stored: Vec<String> = stmt
        .query_map(params![workspace_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    for name in stored {
        if !current_names.contains(&name) {
            conn.execute(
                "DELETE FROM crate_nodes WHERE workspace_id = ?1 AND name = ?2",
                params![workspace_id, name],
            )?;
        }
    }
    Ok(())
}

/// Delete crate dependency edges for a workspace whose exact `(from_crate,
/// to_crate)` pair is no longer present in a freshly parsed
/// [`crate::graph::crates::parse_multi_manifest_workspace`] result.
///
/// Compares the full edge identity, not just crate-name membership: an
/// edge's `to_crate` is very often an external registry dependency (e.g.
/// `tokio`) that never appears in `crate_nodes` at all, so pruning by
/// `to_crate` name membership alone would wrongly delete every edge to a
/// non-workspace-member dependency. A stored edge survives as long as the
/// current parse still reports that exact dependency; it is deleted only
/// when it genuinely no longer exists (the source crate was removed, or it
/// dropped that dependency).
pub fn delete_stale_crate_dep_edges(
    conn: &Connection,
    workspace_id: &str,
    current_edges: &HashSet<(String, String)>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT from_crate, to_crate FROM crate_dep_edges WHERE workspace_id = ?1",
    )?;
    let stored: Vec<(String, String)> = stmt
        .query_map(params![workspace_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    for (from_crate, to_crate) in stored {
        if !current_edges.contains(&(from_crate.clone(), to_crate.clone())) {
            conn.execute(
                "DELETE FROM crate_dep_edges WHERE workspace_id = ?1 AND from_crate = ?2 AND to_crate = ?3",
                params![workspace_id, from_crate, to_crate],
            )?;
        }
    }
    Ok(())
}

/// Delete file records (and cascade-delete their symbols) for files no
/// longer on disk. Also removes each file's own `call_edges`/
/// `type_use_edges` rows via [`delete_file_edges`] first — see that
/// function's doc comment for why `file_records`' cascade alone isn't
/// enough — and returns the union of every deleted file's [`RemovedFileRefs`]
/// for `index::indexer`'s scoped Phase 7 recompute.
pub fn delete_stale_files(
    conn: &Connection,
    workspace_id: &str,
    current_files: &HashSet<String>,
) -> rusqlite::Result<RemovedFileRefs> {
    let mut removed = RemovedFileRefs::default();
    let mut stmt = conn.prepare(
        "SELECT file_path FROM file_records WHERE workspace_id = ?1",
    )?;
    let stored: Vec<String> = stmt
        .query_map(params![workspace_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    for path in stored {
        if !current_files.contains(&path) {
            removed.merge(delete_file_edges(conn, workspace_id, &path)?);
            conn.execute(
                "DELETE FROM file_records WHERE workspace_id = ?1 AND file_path = ?2",
                params![workspace_id, path],
            )?;
        }
    }
    Ok(removed)
}

/// Delete file records (and cascade-delete their symbols) for exactly the
/// given `paths` — used by [`crate::index::indexer::Indexer::run_changed_paths`]
/// (the watcher's targeted incremental pass) when it has already determined
/// via a direct filesystem check that these specific paths no longer exist,
/// rather than diffing a full disk walk against every stored file the way
/// [`delete_stale_files`] does. Unconditional per given path — deleting a
/// path with no matching row is simply a no-op. Same `call_edges`/
/// `type_use_edges` cleanup as [`delete_stale_files`] via [`delete_file_edges`];
/// returns the union [`RemovedFileRefs`].
pub fn delete_specific_files(
    conn: &Connection,
    workspace_id: &str,
    paths: &HashSet<String>,
) -> rusqlite::Result<RemovedFileRefs> {
    let mut removed = RemovedFileRefs::default();
    for path in paths {
        removed.merge(delete_file_edges(conn, workspace_id, path)?);
        conn.execute(
            "DELETE FROM file_records WHERE workspace_id = ?1 AND file_path = ?2",
            params![workspace_id, path],
        )?;
    }
    Ok(removed)
}

