use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{extract::State, Json};
use mcp_service4agent::router::{DocRouter, RouteDoc};
use serde::Serialize;

use crate::server::state::SharedState;
use crate::HealthStatus;

/// Bound on how long `/ready` will wait for a trivial DB round-trip before
/// answering "not ready". `/index`'s writes go through the independent
/// `BatchWriter`, never the reader pool, so this only ever waits on the
/// reader pool's own N connection slots.
const READY_TIMEOUT: Duration = Duration::from_secs(3);

pub fn router() -> DocRouter<SharedState> {
    DocRouter::new()
        .get(
            "/health",
            get_health,
            RouteDoc::new(
                "Liveness probe — cached DB stats + boot-sequence phase \
                 (migrating/building_snapshot/ready), never queries the database or \
                 blocks on anything migrations/the read-model build could be holding. \
                 Answers instantly from the very first second the listener is bound, \
                 even mid-migration (owner ruling 2026-09-24) — see server::startup.",
            )
            .public(),
        )
        .get(
            "/ready",
            get_ready,
            RouteDoc::new(
                "Readiness probe — 503 with the boot phase before migrations/the read-model \
                 build finish; once ready, a bounded SELECT 1, 503 if the reader pool is \
                 exhausted/unreachable",
            )
            .public(),
        )
}

/// Pure liveness snapshot: never queries the database and never blocks on a
/// lock a long-running request could be holding — it only reads the
/// in-memory cache kept fresh by `server::stats::spawn_stats_refresher`,
/// the boot-sequence phase (a single atomic load — see `server::startup`),
/// and the read model's own `RwLock`-guarded stats map (clone-and-drop-the-
/// lock, same discipline as every other [`crate::model::ReadModel`]
/// reader). Shared by the `GET /health` route and the `mem_health` MCP tool
/// (`server::routes::mcp`) — one code path for this capability.
pub(crate) fn health_snapshot(state: &SharedState) -> HealthStatus {
    let cache = state
        .stats_cache
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let db_path = format!("{}/index.db", state.data_dir);
    let snapshots = state
        .read_model
        .stats()
        .into_iter()
        .map(|summary| crate::types::SnapshotHealth {
            workspace_id: summary.workspace_id,
            symbol_count: summary.stats.symbol_count,
            file_count: summary.stats.file_count,
            call_edge_count: summary.stats.call_edge_count,
            age_secs: summary.age_secs,
            build_ms: summary.stats.build_ms,
            estimated_bytes: summary.stats.estimated_bytes,
        })
        .collect();
    HealthStatus {
        ok: cache.ok,
        db_path,
        workspace_count: cache.workspace_count,
        symbol_count: cache.symbol_count,
        file_count: cache.file_count,
        stats_age_secs: cache.age_secs(),
        phase: state.startup.get().to_string(),
        snapshots,
    }
}

async fn get_health(State(state): State<SharedState>) -> Json<HealthStatus> {
    Json(health_snapshot(&state))
}

#[derive(Serialize)]
struct ReadyStatus {
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Readiness probe: can the store actually answer a query right now?
///
/// Before migrations/the initial read-model build complete (see
/// `server::startup`), this reports not-ready with the current phase
/// WITHOUT touching the database — a query against a not-yet-migrated
/// schema is a meaningless signal, not a real "is it down" probe. Once
/// `Ready`, it checks out one reader-pool connection and runs a trivial
/// `SELECT 1` in `spawn_blocking`, bounding the whole wait with
/// [`READY_TIMEOUT`] — this never waits on a heavy analytics route or
/// `/index`, only for one of the reader pool's own N slots.
async fn get_ready(State(state): State<SharedState>) -> impl IntoResponse {
    if !state.startup.is_ready() {
        let phase = state.startup.get().to_string();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyStatus { ready: false, error: Some(format!("server starting up: phase={phase}")) }),
        );
    }

    let (status, body) = match tokio::time::timeout(READY_TIMEOUT, ready_probe(&state)).await {
        Ok(Ok(())) => (StatusCode::OK, ReadyStatus { ready: true, error: None }),
        Ok(Err(msg)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ReadyStatus { ready: false, error: Some(msg) },
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ReadyStatus { ready: false, error: Some("timed out waiting for the database".into()) },
        ),
    };

    (status, Json(body))
}

async fn ready_probe(state: &SharedState) -> Result<(), String> {
    let guard = state.reader_pool.acquire().await.map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        guard
            .query_row("SELECT 1", [], |_row| Ok(()))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::lock::IndexLocks;
    use crate::model::ReadModel;
    use crate::server::startup::StartupState;
    use crate::server::state::{with_db, AppError, AppState};
    use crate::server::stats::StatsCache;
    use crate::storage::{BatchConfig, BatchWriter, Db, DbConfig, DbPool};
    use std::sync::{Arc, RwLock};
    use std::time::Instant;

    fn test_state() -> SharedState {
        let db = Db::open(&DbConfig::in_memory()).expect("in-memory writer db opens");
        let reader_pool = DbPool::open_with_size(&DbConfig::in_memory(), 8).expect("in-memory reader pool opens");
        let writer = BatchWriter::new(db, BatchConfig::default());
        Arc::new(AppState {
            writer,
            reader_pool,
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
            index_locks: IndexLocks::new(),
            last_index: Arc::new(RwLock::new(None)),
            read_model: ReadModel::new(),
            startup: StartupState::ready(),
        })
    }

    /// `/health` must answer instantly even while a long read holds one of
    /// the reader pool's own connections — reproducing (and proving fixed)
    /// the retired global-mutex implementation's 45s watchdog-timeout
    /// failure mode.
    #[tokio::test]
    async fn health_answers_while_a_reader_pool_connection_is_held() {
        let state = test_state();

        // Simulate a long-running DB read (what count_symbols used to do
        // over the 2.7GB on-disk index) holding one pool connection.
        let (lock_acquired_tx, lock_acquired_rx) = tokio::sync::oneshot::channel();
        let pool = state.reader_pool.clone();
        let hold_task = tokio::spawn(async move {
            let guard = pool.acquire().await.expect("acquire reader pool connection");
            tokio::task::spawn_blocking(move || {
                let _ = lock_acquired_tx.send(());
                std::thread::sleep(Duration::from_millis(1500));
                drop(guard);
            })
            .await
            .expect("hold task must not panic");
        });

        lock_acquired_rx
            .await
            .expect("hold task must signal connection acquisition before we probe /health");

        let started = Instant::now();
        let Json(status) = get_health(State(state.clone())).await;
        let elapsed = started.elapsed();

        assert!(status.ok, "cache-only /health should report ok before any refresh failure");
        assert!(
            elapsed < Duration::from_millis(300),
            "/health took {elapsed:?} while a reader pool connection was held — it must never wait on the DB"
        );

        hold_task.await.expect("hold task must not panic");
    }

    /// `/health` must stay responsive even when many concurrent "slow
    /// handler" calls — more than the runtime has worker threads — are all
    /// going through `with_db`, the way `/ast`, `/graph` and `/usage`
    /// routes do (each checking out its own reader-pool connection).
    ///
    /// The runtime here is pinned to 2 worker threads; 8 concurrent slow
    /// `with_db` calls (each blocking a spawn_blocking thread, not a
    /// worker) must not stop `/health` from answering promptly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_answers_under_many_concurrent_slow_handlers() {
        let state = test_state();
        const CONCURRENT_SLOW_CALLS: usize = 8;

        let mut slow_tasks = Vec::with_capacity(CONCURRENT_SLOW_CALLS);
        for _ in 0..CONCURRENT_SLOW_CALLS {
            let state = state.clone();
            slow_tasks.push(tokio::spawn(async move {
                with_db(&state, |_conn| {
                    std::thread::sleep(Duration::from_millis(200));
                    Ok::<(), AppError>(())
                })
                .await
            }));
        }

        // Let the slow tasks actually start contending for reader-pool
        // slots before probing /health.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let started = Instant::now();
        let Json(status) = get_health(State(state.clone())).await;
        let elapsed = started.elapsed();

        assert!(status.ok);
        assert!(
            elapsed < Duration::from_millis(300),
            "/health took {elapsed:?} with {CONCURRENT_SLOW_CALLS} concurrent slow handlers in flight"
        );

        for task in slow_tasks {
            task.await
                .expect("slow handler task must not panic")
                .expect("with_db must not error for a trivial closure");
        }
    }
}
