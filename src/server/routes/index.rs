use axum::{extract::State, Json};
use nemo_service::router::{DocRouter, RouteDoc};
use crate::index::indexer::Indexer;
use crate::{IndexRequest, IndexResult};
use crate::server::state::{AppError, SharedState};

pub fn router() -> DocRouter<SharedState> {
    DocRouter::new().post(
        "/index",
        post_index,
        RouteDoc::new("Trigger (re-)indexing of a workspace")
            .public()
            .body_schema(
                r#"{"workspace_id":"string","root_path":"string","workspace_name":"string","force":"bool"}"#,
            ),
    )
}

async fn post_index(
    State(state): State<SharedState>,
    Json(req): Json<IndexRequest>,
) -> Result<Json<IndexResult>, AppError> {
    let result = Indexer::run(&state.reader_pool, &state.writer, &req).await?;
    Ok(Json(result))
}
