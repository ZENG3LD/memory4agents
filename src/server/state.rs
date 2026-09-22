use std::sync::{Arc, Mutex, RwLock};
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;
use crate::server::stats::StatsCache;
use crate::CoreError;

pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub data_dir: String,
    /// Last known DB stats for `/health`, refreshed by a background task.
    /// See [`crate::server::stats::spawn_stats_refresher`].
    pub stats_cache: Arc<RwLock<StatsCache>>,
}

pub type SharedState = Arc<AppState>;

#[derive(Debug)]
pub enum AppError {
    Core(CoreError),
    Internal(String),
}

impl From<CoreError> for AppError {
    fn from(e: CoreError) -> Self {
        AppError::Core(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            AppError::Core(CoreError::WorkspaceNotFound { id }) => {
                (StatusCode::NOT_FOUND, format!("workspace '{}' not found", id))
            }
            AppError::Core(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };
        let body = serde_json::json!({"error": msg});
        (status, axum::Json(body)).into_response()
    }
}
