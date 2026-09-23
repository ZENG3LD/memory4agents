use axum::{extract::State, Json};
use mcp_service4agent::router::{DocRouter, RouteDoc};
use crate::index::indexer::Indexer;
use crate::index::job::IndexJobStatus;
use crate::{IndexRequest, IndexResult};
use crate::server::state::{AppError, SharedState};

pub fn router() -> DocRouter<SharedState> {
    DocRouter::new().post(
        "/index",
        post_index,
        RouteDoc::new(
            "Trigger (re-)indexing of a workspace. Waits if a reindex \
             (this call or a watch-triggered incremental pass) is already \
             running for the same workspace_id — at most one Indexer run \
             per workspace at a time.",
        )
        .public()
        .body_schema(
            r#"{"workspace_id":"string","root_path":"string","workspace_name":"string","force":"bool"}"#,
        ),
    )
}

/// Waits on `state.index_locks` before running — the same per-workspace
/// single-flight lock `crate::watch`'s debounced driver uses, so a
/// `POST /index` and a watch-triggered incremental pass for the same
/// workspace never interleave. Chosen behaviour when the lock is already
/// held: wait (not a "409 index in progress" rejection) — a caller-invoked
/// `/index` is a request for the freshest possible index, not a poll, so
/// blocking until it's this call's turn and then running is more useful
/// than making the caller retry.
///
/// Shared by `POST /index` (waits for the result) and `mem_index`'s
/// `"reindex"` action (fires this via [`spawn_reindex_job`] instead of
/// waiting) — one code path for the actual indexing call either way.
pub(crate) async fn post_index_impl(state: &SharedState, req: IndexRequest) -> Result<IndexResult, AppError> {
    let _guard = state.index_locks.acquire(&req.workspace_id).await;
    let result = Indexer::run(&state.reader_pool, &state.writer, &req).await?;
    Ok(result)
}

async fn post_index(State(state): State<SharedState>, Json(req): Json<IndexRequest>) -> Result<Json<IndexResult>, AppError> {
    let result = post_index_impl(&state, req).await?;
    Ok(Json(result))
}

/// Fires [`post_index_impl`] in the background (`tokio::spawn`), recording
/// its outcome in `state.last_index` for a later `mem_index` `"status"`
/// call to read — used only by the `mem_index` MCP tool's `"reindex"`
/// action, which must not block the calling agent for the duration of a
/// full workspace index run (MCP guidance: calls over ~10s should not hold
/// the request open). `POST /index` is unaffected: it still awaits
/// [`post_index_impl`] directly and never touches `last_index`.
pub(crate) fn spawn_reindex_job(state: SharedState, req: IndexRequest) {
    let workspace_id = req.workspace_id.clone();
    *state.last_index.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(IndexJobStatus::Running { workspace_id: workspace_id.clone() });
    tokio::spawn(async move {
        let outcome = post_index_impl(&state, req).await;
        let status = match outcome {
            Ok(result) => IndexJobStatus::Done { workspace_id, result },
            Err(err) => IndexJobStatus::Failed { workspace_id, error: format!("{err:?}") },
        };
        *state.last_index.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(status);
    });
}
