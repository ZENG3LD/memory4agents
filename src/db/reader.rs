use crate::error::CoreError;
use crate::types::{
    CrateNode, DepEdge, FileEdge, FileEdgeKind, Symbol, SymbolKind, Visibility, Workspace,
};
use rusqlite::{params, Connection};

fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<Symbol> {
    let kind_str: String = row.get(4)?;
    let vis_str: String = row.get(5)?;
    Ok(Symbol {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        file_path: row.get(2)?,
        name: row.get(3)?,
        kind: SymbolKind::from_str(&kind_str).unwrap_or_else(|| {
            tracing::warn!("[dig2memory] unknown SymbolKind: {kind_str}");
            SymbolKind::Function
        }),
        visibility: Visibility::from_str(&vis_str).unwrap_or_else(|| {
            tracing::warn!("[dig2memory] unknown Visibility: {vis_str}");
            Visibility::Private
        }),
        line: row.get(6)?,
        col: row.get(7)?,
        parent_name: row.get(8)?,
        signature: row.get(9)?,
        body_hash: row.get(10)?,
        shape_fingerprint: row.get(11).unwrap_or(None),
        shape_fingerprint_loose: row.get(12).unwrap_or(None),
        field_count: row.get::<_, Option<i64>>(13).unwrap_or(None).map(|v| v as u32),
        shape_summary: row.get(14).unwrap_or(None),
        callsite_count: row.get::<_, Option<i64>>(15).unwrap_or(None).map(|v| v as u32).unwrap_or(0),
        type_use_count: row.get::<_, Option<i64>>(16).unwrap_or(None).map(|v| v as u32).unwrap_or(0),
        impl_trait: row.get(17).unwrap_or(None),
    })
}

/// Get the stored mtime for a file, or None if not indexed.
pub fn get_file_mtime(
    conn: &Connection,
    workspace_id: &str,
    file_path: &str,
) -> Result<Option<i64>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT mtime FROM file_records WHERE workspace_id = ?1 AND file_path = ?2",
    )?;
    let result = stmt
        .query_map(params![workspace_id, file_path], |row| row.get(0))?
        .next()
        .transpose()?;
    Ok(result)
}

/// Get all symbols defined in a file.
pub fn get_symbols_in_file(
    conn: &Connection,
    workspace_id: &str,
    file_path: &str,
) -> Result<Vec<Symbol>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
         FROM symbols WHERE workspace_id = ?1 AND file_path = ?2 ORDER BY line",
    )?;
    let symbols = stmt
        .query_map(params![workspace_id, file_path], row_to_symbol)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();
    Ok(symbols)
}

/// Cap on the number of callers a single [`get_callers_of`] call
/// materializes. A legitimate low-definition-count name can still be
/// called from thousands of sites (e.g. a widely used helper) — this keeps
/// one read bounded regardless. Names with an unusually large *definition*
/// count are refused entirely before ever reaching this function — see
/// `server::routes::ast::AMBIGUOUS_DEFINITION_THRESHOLD`.
const MAX_CALLERS: i64 = 1000;

/// Get all symbols that call a given callee name.
///
/// Two `JOIN`-based queries, not one row-fetch per matching `call_edges`
/// row: a common bare name can have well over 100k call sites (measured:
/// `"new"` has ~139k distinct callers in the live `nemo` workspace), and
/// the former per-edge uncached `conn.prepare` + round trip was the
/// dominant cost behind `/ast/callers`'s multi-minute hangs (2026-09-24
/// read-path audit).
///
/// - The precise branch (`caller_symbol_id != 0` — the normal case since
///   `index::indexer::resolve_caller_id` resolves an enclosing symbol for
///   almost every call site) joins `call_edges` straight into `symbols` by
///   id in one query instead of one `SELECT ... WHERE id = ?` per edge.
/// - The legacy fallback (`caller_symbol_id = 0` — a call site outside any
///   resolved enclosing symbol; ~750 rows total in the live DB) still needs
///   "the first symbol in that file by line" per distinct caller file, now
///   computed once via a window function instead of once per edge.
///
/// Results are capped at [`MAX_CALLERS`] (SQL-level `LIMIT`, not a
/// post-hoc truncation of an already-materialized `Vec`).
pub fn get_callers_of(
    conn: &Connection,
    callee_name: &str,
    workspace_id: Option<&str>,
) -> Result<Vec<Symbol>, CoreError> {
    let mut results: Vec<Symbol> = Vec::new();
    let mut seen_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();

    match workspace_id {
        Some(ws) => {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.workspace_id, s.file_path, s.name, s.kind, s.visibility, s.line, s.col, s.parent_name, s.signature, s.body_hash, s.shape_fingerprint, s.shape_fingerprint_loose, s.field_count, s.shape_summary, s.callsite_count, s.type_use_count, s.impl_trait
                 FROM (SELECT DISTINCT caller_symbol_id FROM call_edges WHERE callee_name = ?1 AND workspace_id = ?2 AND caller_symbol_id != 0) e
                 JOIN symbols s ON s.id = e.caller_symbol_id
                 LIMIT ?3",
            )?;
            collect_caller_rows(stmt.query_map(params![callee_name, ws, MAX_CALLERS], row_to_symbol)?, &mut seen_ids, &mut results);
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.workspace_id, s.file_path, s.name, s.kind, s.visibility, s.line, s.col, s.parent_name, s.signature, s.body_hash, s.shape_fingerprint, s.shape_fingerprint_loose, s.field_count, s.shape_summary, s.callsite_count, s.type_use_count, s.impl_trait
                 FROM (SELECT DISTINCT caller_symbol_id FROM call_edges WHERE callee_name = ?1 AND caller_symbol_id != 0) e
                 JOIN symbols s ON s.id = e.caller_symbol_id
                 LIMIT ?2",
            )?;
            collect_caller_rows(stmt.query_map(params![callee_name, MAX_CALLERS], row_to_symbol)?, &mut seen_ids, &mut results);
        }
    }

    let remaining = MAX_CALLERS - results.len() as i64;
    if remaining <= 0 {
        return Ok(results);
    }

    match workspace_id {
        Some(ws) => {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
                 FROM (
                     SELECT s.id, s.workspace_id, s.file_path, s.name, s.kind, s.visibility, s.line, s.col, s.parent_name, s.signature, s.body_hash, s.shape_fingerprint, s.shape_fingerprint_loose, s.field_count, s.shape_summary, s.callsite_count, s.type_use_count, s.impl_trait,
                            ROW_NUMBER() OVER (PARTITION BY s.workspace_id, s.file_path ORDER BY s.line ASC) AS rn
                     FROM symbols s
                     JOIN (SELECT DISTINCT workspace_id, caller_file FROM call_edges WHERE callee_name = ?1 AND workspace_id = ?2 AND caller_symbol_id = 0) e
                       ON e.workspace_id = s.workspace_id AND e.caller_file = s.file_path
                 ) ranked
                 WHERE rn = 1
                 LIMIT ?3",
            )?;
            collect_caller_rows(stmt.query_map(params![callee_name, ws, remaining], row_to_symbol)?, &mut seen_ids, &mut results);
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
                 FROM (
                     SELECT s.id, s.workspace_id, s.file_path, s.name, s.kind, s.visibility, s.line, s.col, s.parent_name, s.signature, s.body_hash, s.shape_fingerprint, s.shape_fingerprint_loose, s.field_count, s.shape_summary, s.callsite_count, s.type_use_count, s.impl_trait,
                            ROW_NUMBER() OVER (PARTITION BY s.workspace_id, s.file_path ORDER BY s.line ASC) AS rn
                     FROM symbols s
                     JOIN (SELECT DISTINCT workspace_id, caller_file FROM call_edges WHERE callee_name = ?1 AND caller_symbol_id = 0) e
                       ON e.workspace_id = s.workspace_id AND e.caller_file = s.file_path
                 ) ranked
                 WHERE rn = 1
                 LIMIT ?2",
            )?;
            collect_caller_rows(stmt.query_map(params![callee_name, remaining], row_to_symbol)?, &mut seen_ids, &mut results);
        }
    }

    Ok(results)
}

/// Drains a `query_map` iterator of [`Symbol`] rows into `results`,
/// deduplicating by id and logging (not failing) an individual row error —
/// shared by both branches of [`get_callers_of`].
fn collect_caller_rows(
    rows: impl Iterator<Item = rusqlite::Result<Symbol>>,
    seen_ids: &mut std::collections::HashSet<i64>,
    results: &mut Vec<Symbol>,
) {
    for r in rows {
        match r {
            Ok(s) => {
                if seen_ids.insert(s.id) {
                    results.push(s);
                }
            }
            Err(e) => tracing::warn!("[dig2memory] row read error: {e}"),
        }
    }
}

/// Get all file edges originating from a file.
pub fn get_deps_of_file(
    conn: &Connection,
    workspace_id: &str,
    file_path: &str,
) -> Result<Vec<FileEdge>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT workspace_id, from_file, to_file, kind
         FROM file_edges WHERE workspace_id = ?1 AND from_file = ?2",
    )?;
    let result = stmt
        .query_map(params![workspace_id, file_path], |row| {
            let kind_str: String = row.get(3)?;
            Ok(FileEdge {
                workspace_id: row.get(0)?,
                from_file: row.get(1)?,
                to_file: row.get(2)?,
                kind: FileEdgeKind::from_str(&kind_str).unwrap_or_else(|| {
                    tracing::warn!("[dig2memory] unknown FileEdgeKind: {kind_str}");
                    FileEdgeKind::UseDecl
                }),
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

fn row_to_crate_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<CrateNode> {
    let is_member: i32 = row.get(5)?;
    Ok(CrateNode {
        workspace_id: row.get(0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        manifest_path: row.get(3)?,
        source_dir: row.get(4)?,
        is_workspace_member: is_member != 0,
    })
}

fn row_to_dep_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<DepEdge> {
    let is_path: i32 = row.get(3)?;
    Ok(DepEdge {
        workspace_id: row.get(0)?,
        from_crate: row.get(1)?,
        to_crate: row.get(2)?,
        is_path_dep: is_path != 0,
        dep_path: row.get(4)?,
    })
}

/// List all crate nodes, optionally filtered by workspace.
pub fn list_crate_nodes(
    conn: &Connection,
    workspace_id: Option<&str>,
) -> Result<Vec<CrateNode>, CoreError> {
    match workspace_id {
        Some(ws) => {
            let mut stmt = conn.prepare(
                "SELECT workspace_id, name, version, manifest_path, source_dir, is_workspace_member
                 FROM crate_nodes WHERE workspace_id = ?1",
            )?;
            let result = stmt
                .query_map(params![ws], row_to_crate_node)?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT workspace_id, name, version, manifest_path, source_dir, is_workspace_member
                 FROM crate_nodes",
            )?;
            let result = stmt
                .query_map([], row_to_crate_node)?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
    }
}

/// List all crate dependency edges, optionally filtered by workspace.
pub fn list_crate_deps(
    conn: &Connection,
    workspace_id: Option<&str>,
) -> Result<Vec<DepEdge>, CoreError> {
    match workspace_id {
        Some(ws) => {
            let mut stmt = conn.prepare(
                "SELECT workspace_id, from_crate, to_crate, is_path_dep, dep_path
                 FROM crate_dep_edges WHERE workspace_id = ?1",
            )?;
            let result = stmt
                .query_map(params![ws], row_to_dep_edge)?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT workspace_id, from_crate, to_crate, is_path_dep, dep_path
                 FROM crate_dep_edges",
            )?;
            let result = stmt
                .query_map([], row_to_dep_edge)?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
    }
}

/// Given a symbol name, find which crate(s) define it.
///
/// Joins symbols with crate_nodes by matching the symbol's file_path against
/// each crate's source_dir prefix.  Returns all (symbol, crate) pairs where
/// the symbol name matches exactly.
pub fn resolve_symbol_to_crate(
    conn: &Connection,
    workspace_id: &str,
    symbol_name: &str,
) -> Result<Vec<(Symbol, CrateNode)>, CoreError> {
    // Pull all matching symbols first.
    let mut sym_stmt = conn.prepare(
        "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
         FROM symbols
         WHERE workspace_id = ?1 AND name = ?2",
    )?;
    let symbols: Vec<Symbol> = sym_stmt
        .query_map(params![workspace_id, symbol_name], row_to_symbol)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();

    if symbols.is_empty() {
        return Ok(vec![]);
    }

    // Pull all workspace member crate nodes for this workspace.
    let mut cn_stmt = conn.prepare(
        "SELECT workspace_id, name, version, manifest_path, source_dir, is_workspace_member
         FROM crate_nodes
         WHERE workspace_id = ?1",
    )?;
    let crate_nodes: Vec<CrateNode> = cn_stmt
        .query_map(params![workspace_id], row_to_crate_node)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();

    let mut results = Vec::new();
    for sym in symbols {
        // Normalise the file path (forward slashes, no leading slash).
        let file_norm = sym.file_path.replace('\\', "/");
        let file_norm = file_norm.trim_start_matches('/');

        // Find the crate whose source_dir is the longest prefix of the file path.
        // This handles nested crates correctly.
        let best_crate = crate_nodes
            .iter()
            .filter(|cn| {
                // Empty source_dir means the crate is at the workspace root.
                if cn.source_dir.is_empty() {
                    return true;
                }
                let dir = cn.source_dir.trim_start_matches('/');
                file_norm.starts_with(dir)
                    && file_norm[dir.len()..].starts_with('/')
            })
            .max_by_key(|cn| cn.source_dir.len());

        if let Some(cn) = best_crate {
            results.push((sym, cn.clone()));
        }
    }

    Ok(results)
}

/// List file edges, optionally filtered by workspace and source file.
pub fn list_file_edges(
    conn: &Connection,
    workspace_id: &str,
    from_file: Option<&str>,
) -> Result<Vec<FileEdge>, CoreError> {
    match from_file {
        Some(ff) => {
            let mut stmt = conn.prepare(
                "SELECT workspace_id, from_file, to_file, kind
                 FROM file_edges WHERE workspace_id = ?1 AND from_file = ?2",
            )?;
            let result = stmt
                .query_map(params![workspace_id, ff], |row| {
                    let kind_str: String = row.get(3)?;
                    Ok(FileEdge {
                        workspace_id: row.get(0)?,
                        from_file: row.get(1)?,
                        to_file: row.get(2)?,
                        kind: FileEdgeKind::from_str(&kind_str).unwrap_or_else(|| {
                    tracing::warn!("[dig2memory] unknown FileEdgeKind: {kind_str}");
                    FileEdgeKind::UseDecl
                }),
                    })
                })?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT workspace_id, from_file, to_file, kind
                 FROM file_edges WHERE workspace_id = ?1",
            )?;
            let result = stmt
                .query_map(params![workspace_id], |row| {
                    let kind_str: String = row.get(3)?;
                    Ok(FileEdge {
                        workspace_id: row.get(0)?,
                        from_file: row.get(1)?,
                        to_file: row.get(2)?,
                        kind: FileEdgeKind::from_str(&kind_str).unwrap_or_else(|| {
                    tracing::warn!("[dig2memory] unknown FileEdgeKind: {kind_str}");
                    FileEdgeKind::UseDecl
                }),
                    })
                })?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
    }
}

/// Get files that depend on a given file (reverse file edges).
pub fn get_reverse_file_deps(
    conn: &Connection,
    workspace_id: &str,
    file_path: &str,
) -> Result<Vec<String>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT from_file FROM file_edges WHERE workspace_id = ?1 AND to_file = ?2",
    )?;
    let result = stmt
        .query_map(params![workspace_id, file_path], |row| row.get(0))?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// Get the most depended-upon files (by in-degree of file edges).
pub fn get_hotspots(
    conn: &Connection,
    workspace_id: Option<&str>,
    limit: u32,
) -> Result<Vec<(String, u32)>, CoreError> {
    match workspace_id {
        Some(ws) => {
            let mut stmt = conn.prepare(
                "SELECT to_file, COUNT(*) as cnt
                 FROM file_edges WHERE workspace_id = ?1 AND to_file LIKE '%.rs'
                 GROUP BY to_file ORDER BY cnt DESC LIMIT ?2",
            )?;
            let result = stmt
                .query_map(params![ws, limit], |row| {
                    let count: i64 = row.get(1)?;
                    Ok((row.get(0)?, count as u32))
                })?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT to_file, COUNT(*) as cnt
                 FROM file_edges WHERE to_file LIKE '%.rs'
                 GROUP BY to_file ORDER BY cnt DESC LIMIT ?1",
            )?;
            let result = stmt
                .query_map(params![limit], |row| {
                    let count: i64 = row.get(1)?;
                    Ok((row.get(0)?, count as u32))
                })?
                .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
                .collect();
            Ok(result)
        }
    }
}

/// Get all symbols with a given name, optionally filtered by workspace and kind.
pub fn get_symbols_by_name(
    conn: &Connection,
    name: &str,
    workspace_id: Option<&str>,
    kind: Option<&str>,
) -> Result<Vec<Symbol>, CoreError> {
    macro_rules! collect_syms {
        ($stmt:expr, $params:expr) => {{
            let mut stmt = conn.prepare($stmt)?;
            let result: Vec<Symbol> = stmt
                .query_map($params, row_to_symbol)?
                .filter_map(|r| match r {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!("[dig2memory] row read error: {e}");
                        None
                    }
                })
                .collect();
            result
        }};
    }

    let symbols = match (workspace_id, kind) {
        (Some(ws), Some(k)) => collect_syms!(
            "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
             FROM symbols WHERE name = ?1 AND workspace_id = ?2 AND kind = ?3",
            params![name, ws, k]
        ),
        (Some(ws), None) => collect_syms!(
            "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
             FROM symbols WHERE name = ?1 AND workspace_id = ?2",
            params![name, ws]
        ),
        (None, Some(k)) => collect_syms!(
            "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
             FROM symbols WHERE name = ?1 AND kind = ?2",
            params![name, k]
        ),
        (None, None) => collect_syms!(
            "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait
             FROM symbols WHERE name = ?1",
            params![name]
        ),
    };
    Ok(symbols)
}

/// A row from a semantic duplicates query.
pub struct SemanticDupRow {
    pub shape_fingerprint: String,
    pub name: String,
    pub file_path: String,
    pub line: u32,
    pub kind: String,
    pub field_count: Option<u32>,
    pub shape_summary: Option<String>,
}

/// Options for semantic duplicates query.
pub struct SemanticDupOptions<'a> {
    pub workspace_id: &'a str,
    pub path_prefix: Option<&'a str>,
    pub kinds: Option<&'a [String]>,
    /// If true, group by shape_fingerprint_loose instead of shape_fingerprint.
    pub loose: bool,
}

/// Get all symbols with a non-null shape fingerprint in a workspace,
/// optionally filtered by path prefix and kind list.
pub fn get_semantic_dup_rows(
    conn: &Connection,
    opts: SemanticDupOptions<'_>,
) -> Result<Vec<SemanticDupRow>, CoreError> {
    let fp_col = if opts.loose {
        "shape_fingerprint_loose"
    } else {
        "shape_fingerprint"
    };

    // Build the kind IN clause string if needed (must outlive `conditions`)
    let kind_clause: Option<String> = opts.kinds.and_then(|ks| {
        if ks.is_empty() {
            None
        } else {
            let placeholders: Vec<String> = (3..3 + ks.len()).map(|i| format!("?{i}")).collect();
            Some(format!("kind IN ({})", placeholders.join(", ")))
        }
    });

    // Build WHERE conditions — strings must live as long as `conditions`
    let fp_not_null = format!("{fp_col} IS NOT NULL");
    let mut conditions: Vec<String> = vec![
        "workspace_id = ?1".to_owned(),
        fp_not_null,
    ];
    if opts.path_prefix.is_some() {
        conditions.push("file_path LIKE ?2".to_owned());
    }
    if let Some(ref kc) = kind_clause {
        conditions.push(kc.clone());
    }

    let sql = format!(
        "SELECT {fp_col}, name, file_path, line, kind, field_count, shape_summary FROM symbols WHERE {} ORDER BY {fp_col}",
        conditions.join(" AND ")
    );

    let mut stmt = conn.prepare(&sql)?;

    // Build params
    let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    param_values.push(Box::new(opts.workspace_id.to_owned()));
    if let Some(prefix) = opts.path_prefix {
        param_values.push(Box::new(format!("{prefix}%")));
    }
    if let Some(ks) = opts.kinds {
        for k in ks {
            param_values.push(Box::new(k.clone()));
        }
    }

    let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
    let rows: Vec<SemanticDupRow> = stmt
        .query_map(refs.as_slice(), |row| {
            let line_i: i64 = row.get(3)?;
            let fc: Option<i64> = row.get(5)?;
            Ok(SemanticDupRow {
                shape_fingerprint: row.get(0)?,
                name: row.get(1)?,
                file_path: row.get(2)?,
                line: line_i as u32,
                kind: row.get(4)?,
                field_count: fc.map(|v| v as u32),
                shape_summary: row.get(6)?,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] semantic dup row error: {e}");
                None
            }
        })
        .collect();

    Ok(rows)
}

/// Get names that appear in >= min_count locations, optionally filtered by workspace and kind.
/// Returns (name, kind, total_count) tuples sorted by total desc.
pub fn get_duplicate_names(
    conn: &Connection,
    workspace_id: &str,
    min_count: u32,
    kind: Option<&str>,
) -> Result<Vec<(String, String, u32)>, CoreError> {
    macro_rules! collect_dupes {
        ($stmt:expr, $params:expr) => {{
            let mut stmt = conn.prepare($stmt)?;
            let result: Vec<(String, String, u32)> = stmt
                .query_map($params, |row| {
                    let cnt: i64 = row.get(2)?;
                    Ok((row.get(0)?, row.get(1)?, cnt as u32))
                })?
                .filter_map(|r| match r {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!("[dig2memory] row read error: {e}");
                        None
                    }
                })
                .collect();
            result
        }};
    }

    let rows = match kind {
        Some(k) => collect_dupes!(
            "SELECT name, kind, COUNT(*) as cnt FROM symbols
             WHERE workspace_id = ?1 AND kind = ?2
             GROUP BY name, kind
             HAVING cnt >= ?3
             ORDER BY cnt DESC",
            params![workspace_id, k, min_count]
        ),
        None => collect_dupes!(
            "SELECT name, kind, COUNT(*) as cnt FROM symbols
             WHERE workspace_id = ?1
             GROUP BY name, kind
             HAVING cnt >= ?2
             ORDER BY cnt DESC",
            params![workspace_id, min_count]
        ),
    };
    Ok(rows)
}

/// Count total symbols, optionally filtered by workspace.
pub fn count_symbols(conn: &Connection, workspace_id: Option<&str>) -> Result<u32, CoreError> {
    match workspace_id {
        Some(ws) => {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM symbols WHERE workspace_id = ?1",
                params![ws],
                |row| row.get(0),
            )?;
            Ok(count as u32)
        }
        None => {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))?;
            Ok(count as u32)
        }
    }
}

/// Count total indexed files, optionally filtered by workspace.
pub fn count_files(conn: &Connection, workspace_id: Option<&str>) -> Result<u32, CoreError> {
    match workspace_id {
        Some(ws) => {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM file_records WHERE workspace_id = ?1",
                params![ws],
                |row| row.get(0),
            )?;
            Ok(count as u32)
        }
        None => {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM file_records",
                [],
                |row| row.get(0),
            )?;
            Ok(count as u32)
        }
    }
}

/// Get a workspace by id.
pub fn get_workspace(conn: &Connection, id: &str) -> Result<Option<Workspace>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, root_path, name, indexed_at FROM workspaces WHERE id = ?1",
    )?;
    let result = stmt
        .query_map(params![id], |row| {
            Ok(Workspace {
                id: row.get(0)?,
                root_path: row.get(1)?,
                name: row.get(2)?,
                indexed_at: row.get(3)?,
            })
        })?
        .next()
        .transpose()?;
    Ok(result)
}

/// List all workspaces.
pub fn list_workspaces(conn: &Connection) -> Result<Vec<Workspace>, CoreError> {
    let mut stmt =
        conn.prepare("SELECT id, root_path, name, indexed_at FROM workspaces")?;
    let result = stmt
        .query_map([], |row| {
            Ok(Workspace {
                id: row.get(0)?,
                root_path: row.get(1)?,
                name: row.get(2)?,
                indexed_at: row.get(3)?,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// Get all symbols in a workspace, optionally filtered by path_prefix and kinds.
///
/// Used by /ast/cluster and /ast/parallel_systems.
pub fn get_all_symbols_filtered(
    conn: &Connection,
    workspace_id: &str,
    path_prefix: Option<&str>,
    kinds: Option<&[&str]>,
) -> Result<Vec<Symbol>, CoreError> {
    let mut conditions: Vec<String> = vec!["workspace_id = ?1".to_owned()];
    if path_prefix.is_some() {
        conditions.push("file_path LIKE ?2".to_owned());
    }
    let kind_clause: Option<String> = kinds.and_then(|ks| {
        if ks.is_empty() {
            None
        } else {
            let base = if path_prefix.is_some() { 3usize } else { 2usize };
            let placeholders: Vec<String> =
                (base..base + ks.len()).map(|i| format!("?{i}")).collect();
            Some(format!("kind IN ({})", placeholders.join(", ")))
        }
    });
    if let Some(ref kc) = kind_clause {
        conditions.push(kc.clone());
    }

    let sql = format!(
        "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE {}",
        conditions.join(" AND ")
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    param_values.push(Box::new(workspace_id.to_owned()));
    if let Some(prefix) = path_prefix {
        param_values.push(Box::new(format!("{prefix}%")));
    }
    if let Some(ks) = kinds {
        for k in ks {
            param_values.push(Box::new((*k).to_owned()));
        }
    }

    let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
    let result: Vec<Symbol> = stmt
        .query_map(refs.as_slice(), row_to_symbol)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// Get symbols by fingerprint for /ast/dead_in_group endpoint.
pub fn get_symbols_by_fingerprint(
    conn: &Connection,
    workspace_id: &str,
    fingerprint: &str,
) -> Result<Vec<Symbol>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE workspace_id = ?1 AND shape_fingerprint = ?2 ORDER BY callsite_count DESC, name",
    )?;
    let result: Vec<Symbol> = stmt
        .query_map(params![workspace_id, fingerprint], row_to_symbol)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// A user of a type (from type_use_edges).
pub struct TypeUser {
    pub from_symbol_id: i64,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub line: u32,
    pub use_kind: String,
}

/// Get all symbols that use a given symbol as a type (by to_symbol_id).
pub fn get_type_users(
    conn: &Connection,
    workspace_id: &str,
    to_symbol_id: i64,
) -> Result<Vec<TypeUser>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT e.from_symbol_id, s.name, s.kind, s.file_path, s.line, e.use_kind
         FROM type_use_edges e
         JOIN symbols s ON s.id = e.from_symbol_id
         WHERE e.workspace_id = ?1 AND e.to_symbol_id = ?2
         ORDER BY s.name",
    )?;
    let result: Vec<TypeUser> = stmt
        .query_map(params![workspace_id, to_symbol_id], |row| {
            let line_i: i64 = row.get(4)?;
            Ok(TypeUser {
                from_symbol_id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                file_path: row.get(3)?,
                line: line_i as u32,
                use_kind: row.get(5)?,
            })
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] type_users row error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// Get dead symbols (type_use_count=0 AND callsite_count=0), optionally filtered by path_prefix and kind.
pub fn get_dead_types(
    conn: &Connection,
    workspace_id: &str,
    path_prefix: Option<&str>,
    kind: Option<&str>,
) -> Result<Vec<Symbol>, CoreError> {
    let mut conditions: Vec<String> = vec![
        "workspace_id = ?1".to_owned(),
        "type_use_count = 0".to_owned(),
        "callsite_count = 0".to_owned(),
    ];
    let mut param_idx = 2usize;
    if path_prefix.is_some() {
        conditions.push(format!("file_path LIKE ?{param_idx}"));
        param_idx += 1;
    }
    if kind.is_some() {
        conditions.push(format!("kind = ?{param_idx}"));
    }

    let sql = format!(
        "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE {} ORDER BY name",
        conditions.join(" AND ")
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    param_values.push(Box::new(workspace_id.to_owned()));
    if let Some(prefix) = path_prefix {
        param_values.push(Box::new(format!("{prefix}%")));
    }
    if let Some(k) = kind {
        param_values.push(Box::new(k.to_owned()));
    }

    let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
    let result: Vec<Symbol> = stmt
        .query_map(refs.as_slice(), row_to_symbol)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] dead_types row error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// Get true orphan symbols: type_use_count=0 AND callsite_count=0 AND
/// the symbol name does not appear as a meaningful `to_symbol_id` in type_use_edges.
///
/// When `exclude_pub_use` is true (the default), `pub_use_decl` edges are NOT
/// counted as real usage — only `field`, `param`, `return`, `body`, `use_decl`,
/// `generic`, `bound` edges count.  This prevents re-exported types from being
/// incorrectly labelled as orphans.
///
/// Stricter than `get_dead_types` which only checks the denormalised counts.
pub fn get_orphan_types(
    conn: &Connection,
    workspace_id: &str,
    path_prefix: Option<&str>,
    kind: Option<&str>,
    exclude_pub_use: bool,
) -> Result<Vec<Symbol>, CoreError> {
    let not_exists_clause = if exclude_pub_use {
        // Only edges that are NOT pub_use_decl count as real usage.
        "NOT EXISTS (SELECT 1 FROM type_use_edges e WHERE e.workspace_id = s.workspace_id AND e.to_symbol_id = s.id AND e.use_kind != 'pub_use_decl')".to_owned()
    } else {
        "NOT EXISTS (SELECT 1 FROM type_use_edges e WHERE e.workspace_id = s.workspace_id AND e.to_symbol_id = s.id)".to_owned()
    };

    let mut conditions: Vec<String> = vec![
        "s.workspace_id = ?1".to_owned(),
        "s.type_use_count = 0".to_owned(),
        "s.callsite_count = 0".to_owned(),
        not_exists_clause,
    ];
    let mut param_idx = 2usize;
    if path_prefix.is_some() {
        conditions.push(format!("s.file_path LIKE ?{param_idx}"));
        param_idx += 1;
    }
    if kind.is_some() {
        conditions.push(format!("s.kind = ?{param_idx}"));
    }

    let sql = format!(
        "SELECT s.id, s.workspace_id, s.file_path, s.name, s.kind, s.visibility, s.line, s.col, s.parent_name, s.signature, s.body_hash, s.shape_fingerprint, s.shape_fingerprint_loose, s.field_count, s.shape_summary, s.callsite_count, s.type_use_count FROM symbols s WHERE {} ORDER BY s.name",
        conditions.join(" AND ")
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    param_values.push(Box::new(workspace_id.to_owned()));
    if let Some(prefix) = path_prefix {
        param_values.push(Box::new(format!("{prefix}%")));
    }
    if let Some(k) = kind {
        param_values.push(Box::new(k.to_owned()));
    }

    let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
    let result: Vec<Symbol> = stmt
        .query_map(refs.as_slice(), row_to_symbol)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] orphan_types row error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// Get top-N symbols by type_use_count (usage hotspots).
pub fn get_usage_hotspots(
    conn: &Connection,
    workspace_id: &str,
    path_prefix: Option<&str>,
    top: u32,
) -> Result<Vec<Symbol>, CoreError> {
    let mut conditions: Vec<String> = vec!["workspace_id = ?1".to_owned()];
    if path_prefix.is_some() {
        conditions.push("file_path LIKE ?2".to_owned());
    }

    let sql = format!(
        "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE {} ORDER BY type_use_count DESC, name LIMIT ?{}",
        conditions.join(" AND "),
        if path_prefix.is_some() { 3 } else { 2 }
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut param_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    param_values.push(Box::new(workspace_id.to_owned()));
    if let Some(prefix) = path_prefix {
        param_values.push(Box::new(format!("{prefix}%")));
    }
    param_values.push(Box::new(top as i64));

    let refs: Vec<&dyn rusqlite::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
    let result: Vec<Symbol> = stmt
        .query_map(refs.as_slice(), row_to_symbol)?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] usage_hotspots row error: {e}");
                None
            }
        })
        .collect();
    Ok(result)
}

/// Get symbols by name, optionally filtered by workspace and path_prefix (for /ast/type_usage).
pub fn get_symbols_by_name_prefix(
    conn: &Connection,
    workspace_id: &str,
    name: &str,
    path_prefix: Option<&str>,
) -> Result<Vec<Symbol>, CoreError> {
    let sql = match path_prefix {
        Some(_) => "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE workspace_id = ?1 AND name = ?2 AND file_path LIKE ?3 ORDER BY file_path, line",
        None => "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE workspace_id = ?1 AND name = ?2 ORDER BY file_path, line",
    };
    let mut stmt = conn.prepare(sql)?;
    let result: Vec<Symbol> = match path_prefix {
        Some(prefix) => stmt
            .query_map(params![workspace_id, name, format!("{prefix}%")], row_to_symbol)?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
        None => stmt
            .query_map(params![workspace_id, name], row_to_symbol)?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
    };
    Ok(result)
}

/// Get all trait-impl symbols for a given trait name (kind=impl AND impl_trait=?).
///
/// `path_prefix` restricts to file paths starting with that prefix.
pub fn get_impls_of_trait(
    conn: &Connection,
    workspace_id: &str,
    trait_name: &str,
    path_prefix: Option<&str>,
) -> Result<Vec<Symbol>, CoreError> {
    let sql = match path_prefix {
        Some(_) => "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE workspace_id = ?1 AND kind = 'impl' AND impl_trait = ?2 AND file_path LIKE ?3 ORDER BY file_path, line",
        None => "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE workspace_id = ?1 AND kind = 'impl' AND impl_trait = ?2 ORDER BY file_path, line",
    };
    let mut stmt = conn.prepare(sql)?;
    let result: Vec<Symbol> = match path_prefix {
        Some(prefix) => stmt
            .query_map(params![workspace_id, trait_name, format!("{prefix}%")], row_to_symbol)?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
        None => stmt
            .query_map(params![workspace_id, trait_name], row_to_symbol)?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
    };
    Ok(result)
}

/// Get all inherent-impl symbols for a given type name (kind=impl AND impl_trait IS NULL AND name=?).
///
/// `path_prefix` restricts to file paths starting with that prefix.
pub fn get_inherent_impls_of_type(
    conn: &Connection,
    workspace_id: &str,
    type_name: &str,
    path_prefix: Option<&str>,
) -> Result<Vec<Symbol>, CoreError> {
    let sql = match path_prefix {
        Some(_) => "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE workspace_id = ?1 AND kind = 'impl' AND impl_trait IS NULL AND name = ?2 AND file_path LIKE ?3 ORDER BY file_path, line",
        None => "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count, impl_trait FROM symbols WHERE workspace_id = ?1 AND kind = 'impl' AND impl_trait IS NULL AND name = ?2 ORDER BY file_path, line",
    };
    let mut stmt = conn.prepare(sql)?;
    let result: Vec<Symbol> = match path_prefix {
        Some(prefix) => stmt
            .query_map(params![workspace_id, type_name, format!("{prefix}%")], row_to_symbol)?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
        None => stmt
            .query_map(params![workspace_id, type_name], row_to_symbol)?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
    };
    Ok(result)
}

/// Implementor summary row: (trait_name, implementor_count, implementors).
pub struct TraitImplSummaryRow {
    pub trait_name: String,
    pub implementor_count: u32,
    pub implementors: Vec<String>,
}

/// Get impl summary: for each trait, list all implementor type names.
///
/// Returns rows sorted by implementor_count desc.
pub fn get_impl_summary(
    conn: &Connection,
    workspace_id: &str,
    path_prefix: Option<&str>,
) -> Result<Vec<TraitImplSummaryRow>, CoreError> {
    // First: fetch all (impl_trait, name) pairs for this workspace.
    let sql = match path_prefix {
        Some(_) => "SELECT impl_trait, name FROM symbols WHERE workspace_id = ?1 AND kind = 'impl' AND impl_trait IS NOT NULL AND file_path LIKE ?2 ORDER BY impl_trait, name",
        None => "SELECT impl_trait, name FROM symbols WHERE workspace_id = ?1 AND kind = 'impl' AND impl_trait IS NOT NULL ORDER BY impl_trait, name",
    };
    let mut stmt = conn.prepare(sql)?;

    let pairs: Vec<(String, String)> = match path_prefix {
        Some(prefix) => stmt
            .query_map(params![workspace_id, format!("{prefix}%")], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
        None => stmt
            .query_map(params![workspace_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("[dig2memory] row read error: {e}");
                    None
                }
            })
            .collect(),
    };

    // Group by trait_name.
    let mut map: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for (trait_name, impl_name) in pairs {
        map.entry(trait_name).or_default().push(impl_name);
    }

    let mut rows: Vec<TraitImplSummaryRow> = map
        .into_iter()
        .map(|(trait_name, implementors)| {
            let implementor_count = implementors.len() as u32;
            TraitImplSummaryRow {
                trait_name,
                implementor_count,
                implementors,
            }
        })
        .collect();

    // Sort by implementor_count desc.
    rows.sort_by_key(|r| std::cmp::Reverse(r.implementor_count));
    Ok(rows)
}
