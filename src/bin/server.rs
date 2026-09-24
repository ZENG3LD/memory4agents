use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use memory4agent::index::lock::IndexLocks;
use memory4agent::model::ReadModel;
use memory4agent::server::config::Config;
#[cfg(feature = "nemo")]
use memory4agent::server::registration;
use memory4agent::server::startup::{run_migrations_and_build_snapshots, StartupState};
use memory4agent::server::state::{AppState, SharedState};
use memory4agent::server::stats::{spawn_stats_refresher, StatsCache};
use memory4agent::server::routes;
use memory4agent::storage::migrations::legacy_schema_migrations;
use memory4agent::storage::{BatchConfig, BatchWriter, Db, DbConfig, DbPool, MigrationRunner};
#[cfg(feature = "nemo")]
use mcp_service4agent::register;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env();

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data_dir).expect("failed to create data dir");

    let db_path = format!("{}/index.db", config.data_dir);

    // The writer's own connection. Never handed to a route handler — only
    // `BatchWriter`'s worker thread (started below) and the background
    // startup sequence touch it directly. Opening it and the reader pool is
    // fast (pragmas only, no schema work) — migrations run later, off the
    // request path, see below.
    let db = Db::open(&DbConfig::new(&db_path)).expect("failed to open writer db");
    let reader_pool = DbPool::open_with_size(&DbConfig::new(&db_path), config.reader_pool_size)
        .expect("failed to open reader pool");
    let writer = BatchWriter::new(db.clone(), BatchConfig::default());

    let state: SharedState = Arc::new(AppState {
        writer,
        reader_pool,
        data_dir: config.data_dir,
        stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
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
    let startup_db = db.clone();
    tokio::spawn(async move {
        let migrations = MigrationRunner::new(legacy_schema_migrations());
        if let Err(e) = run_migrations_and_build_snapshots(startup_state.clone(), startup_db, migrations).await {
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
