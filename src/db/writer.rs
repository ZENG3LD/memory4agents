use crate::search::trigram::trigrams;
use crate::types::{CallEdge, CrateNode, DepEdge, FileEdge, FileRecord, Symbol, TypeUseEdge, Workspace};
use rusqlite::{params, Connection};
use std::collections::HashSet;

/// Insert or update a workspace record.
pub fn upsert_workspace(conn: &Connection, workspace: &Workspace) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO workspaces (id, root_path, name, indexed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
             root_path  = excluded.root_path,
             name       = excluded.name,
             indexed_at = excluded.indexed_at",
        params![
            workspace.id,
            workspace.root_path,
            workspace.name,
            workspace.indexed_at,
        ],
    )?;
    Ok(())
}

/// Insert or update a file record.
pub fn upsert_file_record(conn: &Connection, rec: &FileRecord) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO file_records (workspace_id, file_path, mtime, size, symbol_count)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(workspace_id, file_path) DO UPDATE SET
             mtime        = excluded.mtime,
             size         = excluded.size,
             symbol_count = excluded.symbol_count",
        params![
            rec.workspace_id,
            rec.file_path,
            rec.mtime,
            rec.size,
            rec.symbol_count,
        ],
    )?;
    Ok(())
}

/// Delete all symbols for a given file (cascade removes trigrams; also cleans call/type edges).
pub fn delete_file_symbols(
    conn: &Connection,
    workspace_id: &str,
    file_path: &str,
) -> rusqlite::Result<()> {
    // Collect symbol ids before deleting, so we can remove type_use_edges.
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM symbols WHERE workspace_id = ?1 AND file_path = ?2",
        )?;
        let collected: Vec<i64> = stmt
            .query_map(params![workspace_id, file_path], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        collected
    };

    // Delete type_use_edges where this file's symbols appear on either side.
    for id in &ids {
        conn.execute(
            "DELETE FROM type_use_edges WHERE workspace_id = ?1 AND (from_symbol_id = ?2 OR to_symbol_id = ?2)",
            params![workspace_id, id],
        )?;
    }

    conn.execute(
        "DELETE FROM symbols WHERE workspace_id = ?1 AND file_path = ?2",
        params![workspace_id, file_path],
    )?;
    conn.execute(
        "DELETE FROM call_edges WHERE workspace_id = ?1 AND caller_file = ?2",
        params![workspace_id, file_path],
    )?;
    Ok(())
}

/// Insert a symbol and return its new rowid.
pub fn insert_symbol(conn: &Connection, sym: &Symbol) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO symbols (workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
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
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Insert a type-use edge (ignore on duplicate primary key).
pub fn insert_type_use_edge(conn: &Connection, edge: &TypeUseEdge) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO type_use_edges (workspace_id, from_symbol_id, to_symbol_id, use_kind)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            edge.workspace_id,
            edge.from_symbol_id,
            edge.to_symbol_id,
            edge.use_kind,
        ],
    )?;
    Ok(())
}

/// Update type_use_count for all symbols in a workspace from type_use_edges (in-degree).
pub fn update_type_use_counts(conn: &Connection, workspace_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE symbols
         SET type_use_count = (
             SELECT COUNT(DISTINCT from_symbol_id)
             FROM type_use_edges
             WHERE type_use_edges.workspace_id = symbols.workspace_id
               AND type_use_edges.to_symbol_id = symbols.id
         )
         WHERE workspace_id = ?1",
        params![workspace_id],
    )?;
    Ok(())
}

/// Update callsite_count for all symbols in a workspace from call_edges.
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

/// Compute trigrams for a name and insert them into symbol_trigrams.
pub fn insert_symbol_trigrams(
    conn: &Connection,
    symbol_id: i64,
    name: &str,
) -> rusqlite::Result<()> {
    for tgram in trigrams(name) {
        conn.execute(
            "INSERT OR IGNORE INTO symbol_trigrams (symbol_id, trigram) VALUES (?1, ?2)",
            params![symbol_id, tgram],
        )?;
    }
    Ok(())
}

/// Insert a call edge.
pub fn insert_call_edge(conn: &Connection, edge: &CallEdge) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO call_edges (workspace_id, caller_file, caller_symbol_id, callee_name, line)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            edge.workspace_id,
            edge.caller_file,
            edge.caller_symbol_id,
            edge.callee_name,
            edge.line,
        ],
    )?;
    Ok(())
}

/// Insert or update a file edge.
pub fn upsert_file_edge(conn: &Connection, edge: &FileEdge) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO file_edges (workspace_id, from_file, to_file, kind)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_id, from_file, to_file, kind) DO NOTHING",
        params![
            edge.workspace_id,
            edge.from_file,
            edge.to_file,
            edge.kind.to_string(),
        ],
    )?;
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

/// Delete file records (and cascade-delete their symbols) for files no longer on disk.
pub fn delete_stale_files(
    conn: &Connection,
    workspace_id: &str,
    current_files: &HashSet<String>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT file_path FROM file_records WHERE workspace_id = ?1",
    )?;
    let stored: Vec<String> = stmt
        .query_map(params![workspace_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    for path in stored {
        if !current_files.contains(&path) {
            conn.execute(
                "DELETE FROM file_records WHERE workspace_id = ?1 AND file_path = ?2",
                params![workspace_id, path],
            )?;
        }
    }
    Ok(())
}

