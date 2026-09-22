use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;

use crate::server::state::SharedState;
use crate::HealthStatus;

/// Bound on how long `/ready` will wait for a trivial DB round-trip before
/// answering "not ready". Keeps the endpoint responsive even while a long
/// operation (e.g. `/index`) holds the DB lock.
const READY_TIMEOUT: Duration = Duration::from_secs(3);

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/health", get(get_health))
        .route("/ready", get(get_ready))
}

/// Pure liveness probe. Never queries the database and never blocks on a
/// lock a long-running request could be holding — it only reads the
/// in-memory cache kept fresh by `server::stats::spawn_stats_refresher`.
async fn get_health(State(state): State<SharedState>) -> Json<HealthStatus> {
    let cache = state
        .stats_cache
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let db_path = format!("{}/index.db", state.data_dir);
    Json(HealthStatus {
        ok: cache.ok,
        db_path,
        workspace_count: cache.workspace_count,
        symbol_count: cache.symbol_count,
        file_count: cache.file_count,
        stats_age_secs: cache.age_secs(),
    })
}

#[derive(Serialize)]
struct ReadyStatus {
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Readiness probe: can the store actually answer a query right now?
///
/// Runs a trivial `SELECT 1` in `spawn_blocking` and bounds the wait with
/// [`READY_TIMEOUT`]. If the DB mutex is currently held by a long operation
/// (e.g. a re-index), this returns `503` promptly instead of hanging — the
/// blocking task itself is left to finish in the background.
async fn get_ready(State(state): State<SharedState>) -> impl IntoResponse {
    let db = state.db.clone();
    let query = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let conn = db.lock().map_err(|_| "db lock poisoned".to_string())?;
        conn.query_row("SELECT 1", [], |_row| Ok(()))
            .map_err(|e| e.to_string())
    });

    let (status, body) = match tokio::time::timeout(READY_TIMEOUT, query).await {
        Ok(Ok(Ok(()))) => (StatusCode::OK, ReadyStatus { ready: true, error: None }),
        Ok(Ok(Err(msg))) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ReadyStatus { ready: false, error: Some(msg) },
        ),
        Ok(Err(join_err)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ReadyStatus { ready: false, error: Some(join_err.to_string()) },
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ReadyStatus { ready: false, error: Some("timed out waiting for the database".into()) },
        ),
    };

    (status, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::{with_db, AppError, AppState};
    use crate::server::stats::StatsCache;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Instant;

    fn test_state() -> SharedState {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("in-memory sqlite connection must open");
        Arc::new(AppState {
            db: Arc::new(Mutex::new(conn)),
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
        })
    }

    /// `/health` must answer instantly even while a long DB read holds the
    /// same connection mutex the old implementation's `count_symbols` call
    /// used to lock — reproducing the 45s watchdog-timeout failure mode.
    #[tokio::test]
    async fn health_answers_while_db_lock_is_held() {
        let state = test_state();

        // Simulate a long-running DB read (what count_symbols used to do
        // over the 2.7GB on-disk index) holding the same mutex.
        let (lock_acquired_tx, lock_acquired_rx) = tokio::sync::oneshot::channel();
        let db = state.db.clone();
        let hold_task = tokio::task::spawn_blocking(move || {
            let _conn = db.lock().expect("db lock");
            let _ = lock_acquired_tx.send(());
            std::thread::sleep(Duration::from_millis(1500));
        });

        lock_acquired_rx
            .await
            .expect("hold task must signal lock acquisition before we probe /health");

        let started = Instant::now();
        let Json(status) = get_health(State(state.clone())).await;
        let elapsed = started.elapsed();

        assert!(status.ok, "cache-only /health should report ok before any refresh failure");
        assert!(
            elapsed < Duration::from_millis(300),
            "/health took {elapsed:?} while the DB lock was held — it must never wait on the DB"
        );

        hold_task.await.expect("hold task must not panic");
    }

    /// `/health` must stay responsive even when many concurrent "slow
    /// handler" calls — more than the runtime has worker threads — are all
    /// going through `with_db`, the way `/ast`, `/graph` and `/usage`
    /// routes do since their conversion off direct `state.db.lock()`.
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

        // Let the slow tasks actually start contending for the DB mutex
        // before probing /health.
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
