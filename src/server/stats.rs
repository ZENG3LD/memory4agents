//! Background cache of database-derived stats shown by `/health`.
//!
//! `/health` must answer instantly regardless of database size or load, so it
//! never queries the database directly. Instead this module runs a periodic
//! background task that refreshes a small in-memory cache; the `/health`
//! handler only ever reads that cache.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::reader;
use crate::server::state::{AppState, SharedState};
use crate::CoreError;

/// How often the background task refreshes the cached DB stats.
///
/// This is the only place `/health` stats are computed from the database —
/// the request handler reads the cache this task fills.
pub const STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Last known database stats, refreshed off the request path.
#[derive(Debug, Clone)]
pub struct StatsCache {
    /// Whether the last refresh attempt succeeded. Optimistically `true`
    /// before the first refresh completes (process is alive; we simply
    /// don't have a DB answer yet).
    pub ok: bool,
    pub workspace_count: u32,
    pub symbol_count: u32,
    pub file_count: u32,
    updated_at: Option<Instant>,
}

impl StatsCache {
    /// Empty cache used before the first background refresh completes.
    pub fn empty() -> Self {
        Self {
            ok: true,
            workspace_count: 0,
            symbol_count: 0,
            file_count: 0,
            updated_at: None,
        }
    }

    /// Age of the cached values in seconds, or `None` if never refreshed.
    pub fn age_secs(&self) -> Option<u64> {
        self.updated_at.map(|t| t.elapsed().as_secs())
    }
}

/// Reads the three counts the cache reports, against one checked-out
/// reader-pool connection. Kept as a plain `&Connection` function (not a
/// closure inlined at the call site) so its own `?`-propagated `CoreError`s
/// are easy to read independently of how the connection was obtained.
fn collect_stats(conn: &rusqlite::Connection) -> Result<(u32, u32, u32), CoreError> {
    let workspace_count = reader::list_workspaces(conn)?.len() as u32;
    let symbol_count = reader::count_symbols(conn, None)?;
    let file_count = reader::count_files(conn, None)?;
    Ok((workspace_count, symbol_count, file_count))
}

/// Runs one stats refresh against the database and stores the result in
/// `state.stats_cache`.
///
/// The query itself runs inside [`crate::server::state`]'s reader pool's own
/// `spawn_blocking`, over one checked-out connection, so it never blocks a
/// tokio worker thread and never contends with the single writer or with any
/// other route.
async fn refresh_once(state: &Arc<AppState>) {
    // `collect_stats` returns `CoreError`, not `rusqlite::Error` — wrapped in
    // `Ok(..)` so the closure still satisfies `ReadPool::read_labeled`'s own
    // `rusqlite::Result<T>` contract; flattened right after.
    let outcome = state
        .reader_pool()
        .read_labeled("stats_refresh", None, |_: &Result<(u32, u32, u32), CoreError>| 0u64, |conn| {
            Ok(collect_stats(conn))
        })
        .await;
    let result: Result<(u32, u32, u32), String> = match outcome {
        Ok(inner) => inner.map_err(|e| e.to_string()),
        Err(e) => Err(format!("reader pool: {e}")),
    };

    let mut cache = state
        .stats_cache
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    match result {
        Ok((workspace_count, symbol_count, file_count)) => {
            cache.ok = true;
            cache.workspace_count = workspace_count;
            cache.symbol_count = symbol_count;
            cache.file_count = file_count;
            cache.updated_at = Some(Instant::now());
        }
        Err(msg) => {
            tracing::warn!("[memory4agent] stats refresh failed: {msg}");
            cache.ok = false;
            cache.updated_at = Some(Instant::now());
        }
    }
}

/// Spawns the background task that keeps `state.stats_cache` fresh.
///
/// Runs an immediate refresh, then repeats every [`STATS_REFRESH_INTERVAL`].
/// Never blocks the caller — returns as soon as the task is scheduled.
pub fn spawn_stats_refresher(state: SharedState) {
    tokio::spawn(async move {
        loop {
            refresh_once(&state).await;
            tokio::time::sleep(STATS_REFRESH_INTERVAL).await;
        }
    });
}
