//! Last-known status of a background reindex job, kept in
//! `AppState::last_index` (`server::state`) — filled by the `mem_index`
//! MCP tool's `"reindex"` action (`server::routes::mcp`), which fires
//! `Indexer::run` via `tokio::spawn` rather than waiting for it (unlike
//! `POST /index`, whose synchronous-completion contract is unchanged). The
//! `mem_index` `"status"` action reads this back — see
//! `server::routes::index::spawn_reindex_job`.

use crate::IndexResult;
use serde::Serialize;

/// One workspace's most recently started/finished background reindex job.
/// `None` in `AppState::last_index` means no background job has ever run
/// since this process started (a synchronous `POST /index` call does not
/// populate this — only `mem_index`'s `"reindex"` action does).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state")]
pub enum IndexJobStatus {
    Running { workspace_id: String },
    Done { workspace_id: String, result: IndexResult },
    Failed { workspace_id: String, error: String },
}
