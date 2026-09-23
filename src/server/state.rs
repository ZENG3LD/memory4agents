use std::sync::{Arc, RwLock};
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;
use crate::index::job::IndexJobStatus;
use crate::index::lock::IndexLocks;
use crate::server::stats::StatsCache;
use crate::storage::{BatchWriter, DbPool};
use crate::CoreError;

/// Every HTTP/MCP read (including every heavy analytics route in
/// `server::routes::usage`) runs against `reader_pool`; every write goes
/// through the single `writer`. `storage::Db` — the writer's own connection
/// handle — is never held here directly; only `BatchWriter`'s dedicated
/// worker thread and the migration bootstrap in `bin/server.rs` ever touch
/// it. See `memory4agent/CLAUDE.md`'s "Storage engine" section.
pub struct AppState {
    pub writer: BatchWriter,
    pub reader_pool: DbPool,
    pub data_dir: String,
    /// Last known DB stats for `/health`, refreshed by a background task.
    /// See [`crate::server::stats::spawn_stats_refresher`].
    pub stats_cache: Arc<RwLock<StatsCache>>,
    /// Per-workspace single-flight lock, shared by the `/index` route
    /// handler and `crate::watch`'s debounced driver — at most one
    /// `Indexer::run`/`run_changed_paths` call per workspace id at a time.
    /// See [`crate::index::lock::IndexLocks`].
    pub index_locks: IndexLocks,
    /// Last known status of a `mem_index`-triggered background reindex job
    /// (`server::routes::mcp`) — `None` until the first such job runs. A
    /// synchronous `POST /index` call does not touch this. See
    /// [`crate::index::job::IndexJobStatus`].
    pub last_index: Arc<RwLock<Option<IndexJobStatus>>>,
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

/// Runs `f` against a checked-out reader-pool connection inside
/// `spawn_blocking`, so the connection is acquired and returned to the pool
/// entirely on a blocking-pool thread — never held across an `.await` by the
/// caller, and never contending with any other route or with `/index`.
///
/// This is the standard way every route handler in `server::routes` touches
/// the database; see `/ast`, `/graph`, `/usage` and `/index` routes. Unlike
/// the retired mutex-backed implementation, checking out one of
/// [`DbPool`]'s N connections here only ever blocks the (bounded) set of
/// callers that also need a reader-pool slot at the same instant — it never
/// blocks `/health` (which never touches the database at all) and it never
/// blocks the single writer thread `state.writer` owns.
pub async fn with_db<T, F>(state: &SharedState, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> Result<T, AppError> + Send + 'static,
{
    let guard = state
        .reader_pool
        .acquire()
        .await
        .map_err(|e| AppError::Internal(format!("reader pool: {e}")))?;
    tokio::task::spawn_blocking(move || f(&guard))
        .await
        .map_err(|e| AppError::Internal(format!("task join error: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{BatchConfig, Db, DbConfig, MigrationRunner};
    use std::time::Duration;

    /// Proves the recon's #1 defect (one heavy analytics route locking every
    /// route for its whole duration) is gone: a deliberately slow read
    /// checked out on the reader pool must not block a second, fast read —
    /// each gets its own pool slot instead of contending for one global
    /// mutex — and a write submitted while the slow read is in flight still
    /// commits via the independent `BatchWriter`.
    #[tokio::test]
    async fn slow_read_does_not_block_a_concurrent_read_or_write() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "m4a-state-no-global-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);

        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        db.run_migrations(MigrationRunner::new(vec![crate::storage::Migration::new(
            1,
            "t",
            "CREATE TABLE t (n INTEGER PRIMARY KEY);",
        )]))
        .await
        .expect("migration applies");

        let reader_pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("pool opens");
        let writer = BatchWriter::new(db.clone(), BatchConfig::default());
        let state: SharedState = Arc::new(AppState {
            writer: writer.clone(),
            reader_pool,
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
            index_locks: IndexLocks::new(),
            last_index: Arc::new(RwLock::new(None)),
        });

        // Kick off a deliberately slow read on the pool (simulates a heavy
        // usage.rs analytics route) and wait for it to actually start before
        // probing anything else, so the race is real.
        let (slow_started_tx, slow_started_rx) = tokio::sync::oneshot::channel();
        let slow_state = state.clone();
        let slow_task = tokio::spawn(async move {
            with_db(&slow_state, move |conn| {
                let _ = slow_started_tx.send(());
                std::thread::sleep(Duration::from_millis(800));
                conn.query_row("SELECT 1", [], |_row| Ok(()))
                    .map_err(|e| AppError::Internal(e.to_string()))
            })
            .await
        });
        slow_started_rx.await.expect("slow read must signal it started");

        // A second, fast read must complete promptly — it gets its own
        // reader-pool slot instead of waiting on the slow read's lock.
        let started = std::time::Instant::now();
        with_db(&state, |conn| {
            conn.query_row("SELECT 1", [], |_row| Ok(()))
                .map_err(|e| AppError::Internal(e.to_string()))
        })
        .await
        .expect("fast concurrent read must succeed");
        let fast_elapsed = started.elapsed();
        assert!(
            fast_elapsed < Duration::from_millis(300),
            "concurrent read took {fast_elapsed:?} while a slow read was in flight — global lock still present"
        );

        // A `/health`-style read (the in-memory stats cache, never the
        // database) must also answer instantly.
        let health_started = std::time::Instant::now();
        let cache_snapshot = state
            .stats_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ok;
        let health_elapsed = health_started.elapsed();
        assert!(cache_snapshot, "stats cache should report ok before any refresh failure");
        assert!(
            health_elapsed < Duration::from_millis(50),
            "health-style cache read took {health_elapsed:?} — must never wait on the database"
        );

        // A write submitted while the slow read is still in flight must
        // commit — the writer is a completely separate connection/thread.
        writer.send(Box::new(|conn| conn.execute("INSERT INTO t VALUES (1)", []).map(|_| ())));
        assert!(writer.barrier_async().await, "write must commit via the independent BatchWriter");

        let row_count: i64 = with_db(&state, |conn| {
            conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
                .map_err(|e| AppError::Internal(e.to_string()))
        })
        .await
        .expect("post-write read must succeed");
        assert_eq!(row_count, 1, "write submitted during the slow read must be visible once committed");

        slow_task.await.expect("slow read task must not panic").expect("slow read must eventually succeed");

        drop(writer);
        drop(state);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
