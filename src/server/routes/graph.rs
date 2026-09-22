use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use std::collections::{HashSet, VecDeque};
use crate::db::reader;
use crate::server::state::{with_db, AppError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/graph/crates", get(list_crates))
        .route("/graph/files", get(files_in_crate))
        .route("/graph/impact", get(impact_analysis))
        .route("/graph/hotspots", get(hotspots))
        .route("/graph/resolve", get(resolve_symbol))
}

#[derive(Deserialize)]
struct WorkspaceQuery {
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct CrateQuery {
    #[serde(rename = "crate")]
    crate_name: String,
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct FileQuery {
    file: String,
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct HotspotsQuery {
    workspace: Option<String>,
    limit: Option<u32>,
}

async fn list_crates(
    State(state): State<SharedState>,
    Query(params): Query<WorkspaceQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let value = with_db(&state, move |db| {
        let nodes = reader::list_crate_nodes(db, params.workspace.as_deref())?;
        let edges = reader::list_crate_deps(db, params.workspace.as_deref())?;
        Ok(serde_json::json!({ "nodes": nodes, "edges": edges }))
    })
    .await?;
    Ok(Json(value))
}

async fn files_in_crate(
    State(state): State<SharedState>,
    Query(params): Query<CrateQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let value = with_db(&state, move |db| {
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
    .await?;
    Ok(Json(value))
}

async fn impact_analysis(
    State(state): State<SharedState>,
    Query(params): Query<FileQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let value = with_db(&state, move |db| {
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
    .await?;
    Ok(Json(value))
}

async fn hotspots(
    State(state): State<SharedState>,
    Query(params): Query<HotspotsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let value = with_db(&state, move |db| {
        let limit = params.limit.unwrap_or(20);
        let result = reader::get_hotspots(db, params.workspace.as_deref(), limit)?;
        Ok(serde_json::json!(result))
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct ResolveQuery {
    sym: String,
    workspace: String,
}

/// `GET /graph/resolve?sym=ExchangeError&workspace=nemo`
///
/// Returns all symbols matching `sym` along with the crate that owns each one.
async fn resolve_symbol(
    State(state): State<SharedState>,
    Query(params): Query<ResolveQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let value = with_db(&state, move |db| {
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
    .await?;
    Ok(Json(value))
}
