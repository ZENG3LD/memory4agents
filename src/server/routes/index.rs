use axum::{extract::State, Json};
use nemo_service::router::{DocRouter, RouteDoc};
use crate::index::indexer::Indexer;
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
async fn post_index(
    State(state): State<SharedState>,
    Json(req): Json<IndexRequest>,
) -> Result<Json<IndexResult>, AppError> {
    let _guard = state.index_locks.acquire(&req.workspace_id).await;
    let result = Indexer::run(&state.reader_pool, &state.writer, &req).await?;
    Ok(Json(result))
}
