use axum::{
    extract::{Query, State},
    Json,
};
use mcp_service4agent::router::{DocRouter, RouteDoc};
use serde::Deserialize;
use std::collections::{HashSet, VecDeque};
use crate::db::reader;
use crate::server::state::{with_db, AppError, SharedState};

/// Depth cap for `/graph/impact_symbol`'s caller BFS — mirrors `/ast/cluster`'s
/// own depth cap (`server/routes/ast.rs`).
const MAX_IMPACT_SYMBOL_DEPTH: u32 = 4;
const DEFAULT_IMPACT_SYMBOL_DEPTH: u32 = 2;

pub fn router() -> DocRouter<SharedState> {
    DocRouter::new()
        .get("/graph/crates", list_crates, RouteDoc::new("Crate dependency tree (?workspace=WS)").public())
        .get(
            "/graph/files",
            files_in_crate,
            RouteDoc::new("File-level dependency edges within a crate (?crate=NAME&workspace=WS)").public(),
        )
        .get(
            "/graph/impact",
            impact_analysis,
            RouteDoc::new("Impact analysis — what breaks if this file changes (?file=PATH&workspace=WS)").public(),
        )
        .get(
            "/graph/impact_symbol",
            impact_analysis_symbol,
            RouteDoc::new(
                "Symbol-level impact: BFS over callers, N hops \
                 (?sym=NAME&workspace=WS&depth=N, default 2, max 4)",
            )
            .public(),
        )
        .get("/graph/hotspots", hotspots, RouteDoc::new("Most-connected modules (?workspace=WS)").public())
        .get(
            "/graph/resolve",
            resolve_symbol,
            RouteDoc::new("Resolve which crate owns a symbol (?sym=NAME&workspace=WS)").public(),
        )
}

#[derive(Deserialize)]
pub(crate) struct WorkspaceQuery {
    pub(crate) workspace: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct CrateQuery {
    #[serde(rename = "crate")]
    pub(crate) crate_name: String,
    pub(crate) workspace: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    pub(crate) file: String,
    pub(crate) workspace: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct HotspotsQuery {
    pub(crate) workspace: Option<String>,
    pub(crate) limit: Option<u32>,
}

pub(crate) async fn list_crates_impl(
    state: &SharedState,
    params: WorkspaceQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let nodes = reader::list_crate_nodes(db, params.workspace.as_deref())?;
        let edges = reader::list_crate_deps(db, params.workspace.as_deref())?;
        Ok(serde_json::json!({ "nodes": nodes, "edges": edges }))
    })
    .await
}

async fn list_crates(
    State(state): State<SharedState>,
    Query(params): Query<WorkspaceQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(list_crates_impl(&state, params).await?))
}

pub(crate) async fn files_in_crate_impl(
    state: &SharedState,
    params: CrateQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let workspace = params.workspace.as_deref().unwrap_or("");
        let result = reader::list_file_edges(db, workspace, None)?;
        // Filter to edges that belong to the requested crate by crate name prefix
        let filtered: Vec<_> = result
            .into_iter()
            .filter(|e| {
                e.from_file.contains(&params.crate_name) || e.to_file.contains(&params.crate_name)
            })
            .collect();
        Ok(serde_json::json!(filtered))
    })
    .await
}

async fn files_in_crate(
    State(state): State<SharedState>,
    Query(params): Query<CrateQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(files_in_crate_impl(&state, params).await?))
}

pub(crate) async fn impact_analysis_impl(
    state: &SharedState,
    params: FileQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let workspace = params.workspace.as_deref().unwrap_or("");

        // BFS over reverse file dependencies
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(params.file.clone());
        visited.insert(params.file.clone());

        while let Some(current) = queue.pop_front() {
            let reverse_deps = reader::get_reverse_file_deps(db, workspace, &current)?;
            for dep in reverse_deps {
                if visited.insert(dep.clone()) {
                    queue.push_back(dep);
                }
            }
        }

        // Remove the seed file itself from results — callers only want affected files
        visited.remove(&params.file);
        let affected: Vec<String> = visited.into_iter().collect();
        Ok(serde_json::json!({ "file": params.file, "affected": affected }))
    })
    .await
}

async fn impact_analysis(
    State(state): State<SharedState>,
    Query(params): Query<FileQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(impact_analysis_impl(&state, params).await?))
}

#[derive(Deserialize)]
pub(crate) struct SymbolImpactQuery {
    pub(crate) sym: String,
    pub(crate) workspace: Option<String>,
    pub(crate) depth: Option<u32>,
}

/// Symbol-level impact analysis: BFS over `call_edges` outward from `sym`'s
/// direct and transitive callers, up to `depth` hops (default
/// [`DEFAULT_IMPACT_SYMBOL_DEPTH`], capped at [`MAX_IMPACT_SYMBOL_DEPTH`]).
/// Additive alongside the existing file-level `/graph/impact` above — this
/// does not replace it.
///
/// Resolution is name-based (via [`reader::get_callers_of`], the same
/// lookup `/ast/callers` uses) until SCIP lands in phase 2: a caller whose
/// `call_edges` row already carries a resolved `caller_symbol_id` is looked
/// up precisely by id, so two symbols sharing a name in the same file are
/// never conflated with each other. The callee side is still matched by
/// bare name (`callee_name = sym`), scoped to `workspace` when given — a
/// second, unrelated definition with the exact same name in the SAME
/// workspace is indistinguishable from the intended one until SCIP can
/// disambiguate by qualified path.
pub(crate) async fn impact_analysis_symbol_impl(
    state: &SharedState,
    params: SymbolImpactQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let workspace = params.workspace.as_deref();
        let depth = params
            .depth
            .unwrap_or(DEFAULT_IMPACT_SYMBOL_DEPTH)
            .min(MAX_IMPACT_SYMBOL_DEPTH);

        let mut visited_names: HashSet<String> = HashSet::new();
        visited_names.insert(params.sym.clone());
        let mut seen_ids: HashSet<i64> = HashSet::new();
        let mut affected: Vec<crate::types::Symbol> = Vec::new();
        let mut frontier: Vec<String> = vec![params.sym.clone()];

        for _ in 0..depth {
            let mut next_frontier: Vec<String> = Vec::new();
            for name in &frontier {
                let callers = reader::get_callers_of(db, name, workspace)?;
                for caller in callers {
                    if !seen_ids.insert(caller.id) {
                        continue;
                    }
                    if visited_names.insert(caller.name.clone()) {
                        next_frontier.push(caller.name.clone());
                    }
                    affected.push(caller);
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }

        let mut affected_files: Vec<String> = affected.iter().map(|s| s.file_path.clone()).collect();
        affected_files.sort();
        affected_files.dedup();

        let affected_symbols: Vec<serde_json::Value> = affected
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "file": s.file_path,
                    "line": s.line,
                    "kind": s.kind.to_string(),
                })
            })
            .collect();

        Ok(serde_json::json!({
            "symbol": params.sym,
            "affected_symbols": affected_symbols,
            "affected_files": affected_files,
        }))
    })
    .await
}

async fn impact_analysis_symbol(
    State(state): State<SharedState>,
    Query(params): Query<SymbolImpactQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(impact_analysis_symbol_impl(&state, params).await?))
}

pub(crate) async fn hotspots_impl(
    state: &SharedState,
    params: HotspotsQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let limit = params.limit.unwrap_or(20);
        let result = reader::get_hotspots(db, params.workspace.as_deref(), limit)?;
        Ok(serde_json::json!(result))
    })
    .await
}

async fn hotspots(
    State(state): State<SharedState>,
    Query(params): Query<HotspotsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(hotspots_impl(&state, params).await?))
}

#[derive(Deserialize)]
pub(crate) struct ResolveQuery {
    pub(crate) sym: String,
    pub(crate) workspace: String,
}

/// Returns all symbols matching `sym` along with the crate that owns each one.
pub(crate) async fn resolve_symbol_impl(
    state: &SharedState,
    params: ResolveQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let hits = reader::resolve_symbol_to_crate(db, &params.workspace, &params.sym)?;
        let results: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|(sym, crate_node)| {
                serde_json::json!({
                    "symbol": sym,
                    "crate": crate_node,
                })
            })
            .collect();
        Ok(serde_json::json!({ "sym": params.sym, "results": results }))
    })
    .await
}

async fn resolve_symbol(
    State(state): State<SharedState>,
    Query(params): Query<ResolveQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(resolve_symbol_impl(&state, params).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::writer;
    use crate::index::lock::IndexLocks;
    use crate::server::state::AppState;
    use crate::server::stats::StatsCache;
    use crate::storage::{BatchConfig, BatchWriter, Db, DbConfig, DbPool, MigrationRunner};
    use crate::types::{CallEdge, FileRecord, Symbol, SymbolKind, Visibility};
    use std::sync::{Arc, RwLock};

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "m4a-graph-impact-symbol-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn test_symbol(workspace_id: &str, file_path: &str, name: &str) -> Symbol {
        Symbol {
            id: 0,
            workspace_id: workspace_id.to_string(),
            file_path: file_path.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            visibility: Visibility::Public,
            line: 1,
            col: 0,
            parent_name: None,
            signature: None,
            body_hash: None,
            shape_fingerprint: None,
            shape_fingerprint_loose: None,
            field_count: None,
            shape_summary: None,
            callsite_count: 0,
            type_use_count: 0,
            impl_trait: None,
        }
    }

    /// Seeds one workspace `ws` with:
    ///   - `a` (a.rs) — the seed symbol.
    ///   - `b` (b.rs) — calls `a` directly.
    ///   - `b_sibling` (b.rs) — an UNRELATED symbol in the SAME FILE as `b`
    ///     that never calls anything. Its presence, and its absence from
    ///     every assertion below, is what proves impact_symbol resolves
    ///     callers of the specific symbol `b` (via `call_edges`'s resolved
    ///     `caller_symbol_id`) rather than every symbol in `b`'s file.
    ///   - `c` (c.rs) — calls `b` (transitive caller of `a`, depth 2).
    ///
    /// Plus a second, unrelated workspace `other-ws` with its own symbol
    /// named `b` that also calls `a`, proving workspace scoping.
    async fn seeded_state() -> (SharedState, std::path::PathBuf) {
        let path = temp_db_path("bfs");
        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        db.run_migrations(MigrationRunner::new(crate::storage::migrations::legacy_schema_migrations()))
            .await
            .expect("migration applies");

        let write_db = db.clone();
        tokio::task::spawn_blocking(move || {
            write_db.write_blocking(|conn| {
                for (ws, file) in [("ws", "a.rs"), ("ws", "b.rs"), ("ws", "c.rs"), ("other-ws", "x.rs")] {
                    writer::upsert_file_record(
                        conn,
                        &FileRecord {
                            workspace_id: ws.to_string(),
                            file_path: file.to_string(),
                            mtime: 0,
                            size: 0,
                            symbol_count: 0,
                        },
                    )?;
                }

                writer::insert_symbol(conn, &test_symbol("ws", "a.rs", "a"))?;
                let b_id = writer::insert_symbol(conn, &test_symbol("ws", "b.rs", "b"))?;
                writer::insert_symbol(conn, &test_symbol("ws", "b.rs", "b_sibling"))?;
                let c_id = writer::insert_symbol(conn, &test_symbol("ws", "c.rs", "c"))?;
                let other_ws_b = writer::insert_symbol(conn, &test_symbol("other-ws", "x.rs", "b"))?;

                writer::insert_call_edge(
                    conn,
                    &CallEdge {
                        workspace_id: "ws".to_string(),
                        caller_file: "b.rs".to_string(),
                        caller_symbol_id: b_id,
                        callee_name: "a".to_string(),
                        line: 1,
                    },
                )?;
                writer::insert_call_edge(
                    conn,
                    &CallEdge {
                        workspace_id: "ws".to_string(),
                        caller_file: "c.rs".to_string(),
                        caller_symbol_id: c_id,
                        callee_name: "b".to_string(),
                        line: 1,
                    },
                )?;
                writer::insert_call_edge(
                    conn,
                    &CallEdge {
                        workspace_id: "other-ws".to_string(),
                        caller_file: "x.rs".to_string(),
                        caller_symbol_id: other_ws_b,
                        callee_name: "a".to_string(),
                        line: 1,
                    },
                )?;
                Ok(())
            })
        })
        .await
        .expect("join")
        .expect("seed writes succeed");

        let reader_pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("reader pool opens");
        let writer_handle = BatchWriter::new(db, BatchConfig::default());
        let state: SharedState = Arc::new(AppState {
            writer: writer_handle,
            reader_pool,
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
            index_locks: IndexLocks::new(),
            last_index: Arc::new(RwLock::new(None)),
        });
        (state, path)
    }

    #[tokio::test]
    async fn impact_symbol_returns_transitive_callers_scoped_to_workspace_and_symbol() {
        let (state, path) = seeded_state().await;

        let Json(value) = impact_analysis_symbol(
            State(state.clone()),
            Query(SymbolImpactQuery {
                sym: "a".to_string(),
                workspace: Some("ws".to_string()),
                depth: None,
            }),
        )
        .await
        .expect("impact_symbol succeeds");

        let affected_names: Vec<String> = value["affected_symbols"]
            .as_array()
            .expect("affected_symbols is an array")
            .iter()
            .map(|s| s["name"].as_str().expect("name field is a string").to_string())
            .collect();

        assert!(
            affected_names.contains(&"b".to_string()),
            "direct caller 'b' must be present: {affected_names:?}"
        );
        assert!(
            affected_names.contains(&"c".to_string()),
            "transitive caller 'c' (depth 2) must be present: {affected_names:?}"
        );
        assert_eq!(
            affected_names.len(),
            2,
            "must exclude b's unrelated file-sibling and the other workspace's same-named symbol: {affected_names:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn impact_symbol_depth_one_stops_at_the_direct_caller() {
        let (state, path) = seeded_state().await;

        let Json(value) = impact_analysis_symbol(
            State(state.clone()),
            Query(SymbolImpactQuery {
                sym: "a".to_string(),
                workspace: Some("ws".to_string()),
                depth: Some(1),
            }),
        )
        .await
        .expect("impact_symbol succeeds");

        let affected_names: Vec<String> = value["affected_symbols"]
            .as_array()
            .expect("affected_symbols is an array")
            .iter()
            .map(|s| s["name"].as_str().expect("name field is a string").to_string())
            .collect();

        assert_eq!(
            affected_names,
            vec!["b".to_string()],
            "depth=1 must return only the direct caller, not the transitive one: {affected_names:?}"
        );

        let _ = std::fs::remove_file(&path);
    }
}
