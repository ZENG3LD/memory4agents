use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use memory4agent::db::legacy_schema::legacy_schema_migrations;
use memory4agent::index::lock::IndexLocks;
use memory4agent::model::ReadModel;
use memory4agent::server::checkpoint::{spawn_checkpoint_task, CheckpointCache};
use memory4agent::server::config::Config;
#[cfg(feature = "nemo")]
use memory4agent::server::registration;
use memory4agent::server::startup::{run_migrations_and_build_snapshots, StartupState};
use memory4agent::server::state::{AppState, SharedState, Storage};
use memory4agent::server::stats::{spawn_stats_refresher, StatsCache};
use memory4agent::server::routes;
use mcp_service4agent::storage::{BatchConfig, BatchWriter, Checkpointer, Db, DbConfig, MigrationRunner, ReadPoolConfig};
#[cfg(feature = "nemo")]
use mcp_service4agent::register;

/// Total page-cache budget the reader pool spends across all its
/// connections — see `mcp_service4agent::storage::ReadPoolConfig`.
const READER_POOL_CACHE_BUDGET_KIB: i64 = 256 * 1024;

/// Default per-call read deadline for reads through the reader pool
/// (`server::state::with_db` and every route built on it) — shorter than
/// the crate's own library default (30s) since this pool serves interactive
/// HTTP/MCP routes. The read-model snapshot build overrides this per-call
/// with its own much longer deadline — see `model::read_model::build_and_set`.
const READER_POOL_DEFAULT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env();

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data_dir).expect("failed to create data dir");

    let db_path = format!("{}/index.db", config.data_dir);

    // Nothing here touches the database: the handles are opened by the
    // background startup task after the listener is bound (owner ruling
    // 2026-09-24 — `/health` and the database work never block each other;
    // opening alone can take minutes when SQLite replays a large WAL).
    let state: SharedState = Arc::new(AppState {
        storage: std::sync::OnceLock::new(),
        data_dir: config.data_dir.clone(),
        stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
        checkpoint_cache: Arc::new(RwLock::new(CheckpointCache::empty())),
        index_locks: IndexLocks::new(),
        last_index: Arc::new(RwLock::new(None)),
        read_model: ReadModel::new(),
        startup: StartupState::new(),
    });

    let (router, endpoints) = routes::build_router();
    let app = router.with_state(state.clone());

    let addr = format!("127.0.0.1:{}", config.port);
    let listener = TcpListener::bind(&addr).await.expect("failed to bind");
    tracing::info!("memory4agent-server listening on {} (phase=migrating)", addr);

    // Self-registration with the local process supervisor (feature `nemo`):
    // never fails or delays startup; fire-and-forget once the socket is
    // live. Registration doesn't depend on the database, so it happens
    // regardless of migration/snapshot-build progress.
    #[cfg(feature = "nemo")]
    let _reassert = register::spawn(
        registration::service_manifest(endpoints, config.port),
        register::DEFAULT_REASSERT_INTERVAL,
    );
    #[cfg(not(feature = "nemo"))]
    drop(endpoints);

    // Owner ruling (2026-09-24): the listener binds and `/health` answers
    // BEFORE migrations or the initial read-model snapshot build run — a
    // slow migration (measured: a `CREATE INDEX` over ~500k rows took 86s
    // on the live HDD-backed database) must never leave the port closed
    // long enough for d-watchdog to declare the process hung and kill it
    // mid-migration, restarting the whole migration from zero. Every hot
    // route gates on `state.startup.is_ready()` (via `server::state::with_db`
    // and `snapshot`/`all_snapshots`) until this background task reaches
    // `StartupPhase::Ready` — see `server::startup`.
    let startup_state = state.clone();
    let reader_pool_size = config.reader_pool_size;
    let wal_path = std::path::PathBuf::from(format!("{db_path}-wal"));
    tokio::spawn(async move {
        let opened = tokio::task::spawn_blocking(move || {
            let db = Db::open(&DbConfig::new(&db_path)).map_err(|e| format!("writer db: {e}"))?;
            let pool = ReadPoolConfig::new(&db_path)
                .pool_size(reader_pool_size)
                .total_cache_budget_kib(READER_POOL_CACHE_BUDGET_KIB)
                .default_read_deadline(READER_POOL_DEFAULT_DEADLINE)
                .open()
                .map_err(|e| format!("reader pool: {e}"))?;
            // Own dedicated connection for `PRAGMA wal_checkpoint` — off
            // `BatchWriter`'s queue and off the reader pool entirely (see
            // `server::checkpoint`'s own module doc). Sized with the SAME
            // per-connection cache share the reader pool computed for
            // itself, rather than a second, independent constant.
            let checkpointer = Checkpointer::open(&db_path, pool.per_connection_cache_kib())
                .map_err(|e| format!("checkpointer: {e}"))?;
            Ok::<_, String>((db, pool, checkpointer))
        })
        .await;
        let (db, reader_pool, checkpointer) = match opened {
            Ok(Ok(handles)) => handles,
            Ok(Err(e)) => {
                tracing::error!("opening the database failed: {e}");
                return;
            }
            Err(e) => {
                tracing::error!("opening the database panicked: {e}");
                return;
            }
        };
        let writer = BatchWriter::new(db.clone(), BatchConfig::default());
        if startup_state.storage.set(Storage::new(writer, reader_pool)).is_err() {
            tracing::error!("database handles were already set");
            return;
        }
        tracing::info!("database open");

        spawn_checkpoint_task(startup_state.clone(), Arc::new(checkpointer), wal_path);

        let migrations = MigrationRunner::new(legacy_schema_migrations());
        if let Err(e) = run_migrations_and_build_snapshots(startup_state.clone(), db, migrations).await {
            tracing::error!("startup sequence failed: {e}");
            return;
        }

        spawn_stats_refresher(startup_state.clone());

        // File watcher (piece 0.9): debounced incremental reindex. Started
        // only once `Ready` — a watch-triggered reindex touches the same
        // schema migrations just finished preparing. There is no single
        // "the workspace" this server owns by default — every /index
        // caller supplies its own workspace_id/root_path — so the watcher
        // only starts when a root path is explicitly configured.
        match std::env::var("MEMORY4AGENT_WATCH_ROOT_PATH") {
            Ok(watch_root) => {
                let watch_workspace_id = std::env::var("MEMORY4AGENT_WATCH_WORKSPACE_ID")
                    .unwrap_or_else(|_| "nemo".to_string());
                memory4agent::watch::spawn_watcher(
                    startup_state,
                    watch_workspace_id,
                    watch_root,
                    memory4agent::watch::WatchConfig::from_env(),
                );
            }
            Err(_) => tracing::info!("MEMORY4AGENT_WATCH_ROOT_PATH unset — file watcher not started"),
        }
    });

    axum::serve(listener, app).await.expect("server error");
}
