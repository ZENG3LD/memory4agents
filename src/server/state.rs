use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;
use crate::index::job::IndexJobStatus;
use crate::index::lock::IndexLocks;
use crate::model::{ReadModel, Snapshot};
use crate::server::checkpoint::CheckpointCache;
use crate::server::startup::{StartupPhase, StartupState};
use crate::server::stats::StatsCache;
use mcp_service4agent::storage::{BatchWriter, DbError, ReadPool};
use crate::CoreError;

/// Every HTTP/MCP read (including every heavy analytics route in
/// `server::routes::usage`) runs against `reader_pool`; every write goes
/// through the single `writer`. `storage::Db` — the writer's own connection
/// handle — is never held here directly; only `BatchWriter`'s dedicated
/// worker thread and the migration bootstrap in `bin/server.rs` ever touch
/// it. See `memory4agent/CLAUDE.md`'s "Storage engine" section.
/// The database handles. Opened by the background startup task AFTER the
/// listener is bound — never on the main path: opening can be slow (SQLite
/// replays a large WAL on first open) and must not keep `/health` down.
pub struct Storage {
    pub writer: BatchWriter,
    /// `Arc`-wrapped: unlike the retired `DbPool` (internally `Arc<PoolInner>`
    /// and cheaply `Clone`), `mcp_service4agent::storage::ReadPool` is a
    /// plain, non-`Clone` struct — callers that need an owned, `'static`
    /// handle to move into a spawned task (`model::read_model::request_rebuild`,
    /// `server::routes::health`'s own test) share this `Arc` instead.
    pub reader_pool: Arc<ReadPool>,
    /// Reads through [`with_db`] that came back as
    /// `Err(DbError::Deadline { .. })` — surfaced by `/health`/`mem_health`
    /// alongside `reader_pool.metrics_snapshot()`'s own slow-call counts.
    reader_deadline_hits: Arc<AtomicU64>,
}

impl Storage {
    /// Constructs the storage cell directly. Used by `bin/server.rs`'s own
    /// background startup task, which owns the `OnceLock` itself
    /// (`AppState::storage`) and only needs the value to `.set()` into it.
    ///
    /// `reader_pool` accepts either a freshly-opened [`ReadPool`] (wrapped
    /// in a new `Arc` here) or an already-`Arc`-wrapped one a caller wants
    /// to keep its own handle to (e.g. a test that also polls the pool
    /// directly after handing a clone to `Storage`).
    pub fn new(writer: BatchWriter, reader_pool: impl Into<Arc<ReadPool>>) -> Storage {
        Storage {
            writer,
            reader_pool: reader_pool.into(),
            reader_deadline_hits: Arc::new(AtomicU64::new(0)),
        }
    }

    /// An already-open storage cell, pre-wrapped in a ready `OnceLock` — for
    /// callers that open the database themselves (tests, tools).
    pub fn ready(writer: BatchWriter, reader_pool: impl Into<Arc<ReadPool>>) -> std::sync::OnceLock<Storage> {
        std::sync::OnceLock::from(Self::new(writer, reader_pool))
    }
}

pub struct AppState {
    /// Set once by the startup task; empty while the database is opening.
    /// Routes gate on [`StartupState`] and never read it before `Ready`.
    pub storage: std::sync::OnceLock<Storage>,
    pub data_dir: String,
    /// Last known DB stats for `/health`, refreshed by a background task.
    /// See [`crate::server::stats::spawn_stats_refresher`].
    pub stats_cache: Arc<RwLock<StatsCache>>,
    /// Last known WAL checkpoint outcome for `/health`, refreshed by
    /// [`crate::server::checkpoint::spawn_checkpoint_task`].
    pub checkpoint_cache: Arc<RwLock<CheckpointCache>>,
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
    /// In-memory read model (piece 0.12) — every ported hot route
    /// (`server::routes::ast`, `graph`, `usage::type_usage`) reads from
    /// this instead of `reader_pool`. See [`snapshot`]/[`all_snapshots`].
    pub read_model: ReadModel,
    /// Boot-sequence phase — see `server::startup`. `/health` reports it
    /// without gating on it; every other DB-touching route gates on it via
    /// [`with_db`]/[`snapshot`]/[`all_snapshots`].
    pub startup: StartupState,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    /// The single writer. Only valid once the startup task has opened the
    /// database — every caller runs after `startup.is_ready()`.
    pub fn writer(&self) -> &BatchWriter {
        &self.opened().writer
    }

    /// The reader pool. Same validity rule as [`AppState::writer`]. Returns
    /// the `Arc` (not just `&ReadPool`) so a caller that needs an owned,
    /// `'static` handle to move into a spawned task can `.clone()` it
    /// cheaply — see [`Storage::reader_pool`]'s own doc for why `ReadPool`
    /// itself isn't `Clone`.
    pub fn reader_pool(&self) -> &Arc<ReadPool> {
        &self.opened().reader_pool
    }

    /// Reads through [`with_db`] that hit their deadline, so far — `0`
    /// before the database is open (nothing has read yet). See
    /// `server::routes::health::health_snapshot`.
    pub fn reader_deadline_hits(&self) -> u64 {
        self.storage.get().map(|s| s.reader_deadline_hits.load(Ordering::Relaxed)).unwrap_or(0)
    }

    fn opened(&self) -> &Storage {
        self.storage
            .get()
            .expect("database not open yet: callers must gate on startup.is_ready()")
    }
}

#[derive(Debug)]
pub enum AppError {
    Core(CoreError),
    Internal(String),
    /// The server hasn't finished migrations/the initial read-model build
    /// yet — see `server::startup`. Every DB-touching route (`with_db`,
    /// [`snapshot`], [`all_snapshots`]) returns this instead of touching a
    /// possibly-un-migrated schema or an empty read model.
    NotReady(StartupPhase),
}

impl From<CoreError> for AppError {
    fn from(e: CoreError) -> Self {
        AppError::Core(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Core(CoreError::WorkspaceNotFound { id }) => {
                let msg = format!("workspace '{id}' not found");
                (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error": msg}))).into_response()
            }
            AppError::Core(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": e.to_string()})))
                    .into_response()
            }
            AppError::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": msg}))).into_response()
            }
            AppError::NotReady(phase) => (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "server starting up",
                    "phase": phase.to_string(),
                })),
            )
                .into_response(),
        }
    }
}

/// Runs `f` against a checked-out reader-pool connection, via
/// [`ReadPool::read_labeled`] under the pool's own default read deadline
/// (see `mcp_service4agent::storage::ReadPoolConfig::default_read_deadline`,
/// wired to 10s at boot — `bin/server.rs`) — the connection is acquired and
/// returned to the pool entirely inside the pool's own `spawn_blocking`,
/// never held across an `.await` by the caller, and never contending with
/// any other route or with `/index`.
///
/// This is the standard way every route handler in `server::routes` touches
/// the database; see `/ast`, `/graph`, `/usage` and `/index` routes. Unlike
/// the retired mutex-backed implementation, checking out one of
/// [`ReadPool`]'s N connections here only ever blocks the (bounded) set of
/// callers that also need a reader-pool slot at the same instant — it never
/// blocks `/health` (which never touches the database at all) and it never
/// blocks the single writer thread `state.writer()` owns. A read that
/// outlives the pool's default deadline counts toward
/// [`AppState::reader_deadline_hits`] — surfaced by `/health`/`mem_health`
/// alongside `reader_pool.metrics_snapshot()`'s own slow-call counts.
pub async fn with_db<T, F>(state: &SharedState, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> Result<T, AppError> + Send + 'static,
{
    if !state.startup.is_ready() {
        return Err(AppError::NotReady(state.startup.get()));
    }
    let outcome = state
        .reader_pool()
        .read_labeled("with_db", None, |_: &Result<T, AppError>| 0u64, move |conn| Ok(f(conn)))
        .await;
    match outcome {
        Ok(inner) => inner,
        Err(err @ DbError::Deadline { .. }) => {
            if let Some(storage) = state.storage.get() {
                storage.reader_deadline_hits.fetch_add(1, Ordering::Relaxed);
            }
            Err(AppError::Internal(format!("reader pool: {err}")))
        }
        Err(err) => Err(AppError::Internal(format!("reader pool: {err}"))),
    }
}

/// The read-model counterpart to [`with_db`]: returns the current snapshot
/// for `workspace_id`, gated on [`crate::server::startup::StartupState::is_ready`].
/// Ported hot routes (`server::routes::ast`, `graph`,
/// `usage::type_usage_impl`) call this instead of `with_db`.
///
/// A workspace with no loaded snapshot (never indexed, or indexed for the
/// first time after boot and not yet rebuilt into the read model — see
/// [`crate::model::read_model::request_rebuild`]) resolves to
/// [`crate::model::Snapshot::empty`], NOT an error: every query method
/// against an empty snapshot naturally returns an empty result, matching
/// what the retired SQL functions returned for a `WHERE workspace_id = ?`
/// scan over an unknown/never-indexed workspace id.
pub fn snapshot(state: &SharedState, workspace_id: &str) -> Result<Arc<Snapshot>, AppError> {
    if !state.startup.is_ready() {
        return Err(AppError::NotReady(state.startup.get()));
    }
    Ok(state.read_model.get(workspace_id).unwrap_or_else(|| Arc::new(Snapshot::empty(workspace_id))))
}

/// Cross-workspace variant for the (rare) `workspace=None` query shape a
/// few routes accept, matching `db::reader`'s own `Option<&str>` convention
/// where `None` means "every workspace".
pub fn all_snapshots(state: &SharedState) -> Result<Vec<Arc<Snapshot>>, AppError> {
    if !state.startup.is_ready() {
        return Err(AppError::NotReady(state.startup.get()));
    }
    Ok(state.read_model.all())
}

/// Convenience wrapper used by every ported route: resolves to a single
/// snapshot when `workspace` is given, or every currently-loaded workspace
/// when it is `None` — the read-model equivalent of calling a `db::reader`
/// function with `workspace_id: Option<&str>`.
pub fn snapshots_for(state: &SharedState, workspace: Option<&str>) -> Result<Vec<Arc<Snapshot>>, AppError> {
    match workspace {
        Some(ws) => Ok(vec![snapshot(state, ws)?]),
        None => all_snapshots(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_service4agent::storage::{BatchConfig, Db, DbConfig, MigrationRunner, ReadPoolConfig};
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
        db.run_migrations(MigrationRunner::new(vec![mcp_service4agent::storage::Migration::new(
            1,
            "t",
            "CREATE TABLE t (n INTEGER PRIMARY KEY);",
        )]))
        .await
        .expect("migration applies");

        let reader_pool = ReadPoolConfig::new(&path).pool_size(4).open().expect("pool opens");
        let writer = BatchWriter::new(db.clone(), BatchConfig::default());
        let state: SharedState = Arc::new(AppState {
            storage: crate::server::state::Storage::ready(writer.clone(), reader_pool),
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
            checkpoint_cache: Arc::new(RwLock::new(crate::server::checkpoint::CheckpointCache::empty())),
            index_locks: IndexLocks::new(),
            last_index: Arc::new(RwLock::new(None)),
            read_model: ReadModel::new(),
            startup: StartupState::ready(),
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
