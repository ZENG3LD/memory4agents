use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use memory4agent::index::lock::IndexLocks;
use memory4agent::server::config::Config;
use memory4agent::server::registration;
use memory4agent::server::state::{AppState, SharedState};
use memory4agent::server::stats::{spawn_stats_refresher, StatsCache};
use memory4agent::server::routes;
use memory4agent::storage::migrations::{legacy_schema_migrations, seed_legacy_versions_if_needed};
use memory4agent::storage::{BatchConfig, BatchWriter, Db, DbConfig, DbPool, MigrationRunner};
use nemo_service::register;

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
    // `BatchWriter`'s worker thread (started below) and this boot sequence
    // touch it directly.
    let db = Db::open(&DbConfig::new(&db_path)).expect("failed to open writer db");

    // One-time backfill for a database written before the versioned-migration
    // rewrite (see `storage::migrations::seed_legacy_versions_if_needed`).
    // No-op on a fresh database or one already carrying `schema_migrations`.
    let seed_db = db.clone();
    tokio::task::spawn_blocking(move || seed_db.write_blocking(|conn| seed_legacy_versions_if_needed(conn)))
        .await
        .expect("legacy schema backfill task panicked")
        .expect("legacy schema backfill failed");

    db.run_migrations(MigrationRunner::new(legacy_schema_migrations()))
        .await
        .expect("failed to run migrations");

    let reader_pool = DbPool::open_with_size(&DbConfig::new(&db_path), config.reader_pool_size)
        .expect("failed to open reader pool");
    let writer = BatchWriter::new(db, BatchConfig::default());

    let state: SharedState = Arc::new(AppState {
        writer,
        reader_pool,
        data_dir: config.data_dir,
        stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
        index_locks: IndexLocks::new(),
        last_index: Arc::new(RwLock::new(None)),
    });

    spawn_stats_refresher(state.clone());

    // File watcher (piece 0.9): debounced incremental reindex. There is no
    // single "the workspace" this server owns by default — every /index
    // caller supplies its own workspace_id/root_path — so the watcher only
    // starts when a root path is explicitly configured.
    match std::env::var("MEMORY4AGENT_WATCH_ROOT_PATH") {
        Ok(watch_root) => {
            let watch_workspace_id = std::env::var("MEMORY4AGENT_WATCH_WORKSPACE_ID")
                .unwrap_or_else(|_| "nemo".to_string());
            memory4agent::watch::spawn_watcher(
                state.clone(),
                watch_workspace_id,
                watch_root,
                memory4agent::watch::WatchConfig::from_env(),
            );
        }
        Err(_) => tracing::info!("MEMORY4AGENT_WATCH_ROOT_PATH unset — file watcher not started"),
    }

    let (router, endpoints) = routes::build_router();
    let app = router.with_state(state);

    let addr = format!("127.0.0.1:{}", config.port);
    tracing::info!("memory4agent-server listening on {}", addr);

    let listener = TcpListener::bind(&addr).await.expect("failed to bind");

    // Self-registration (`nemo-service/CLAUDE.md`'s contract: this can
    // never fail or delay startup) -- fire-and-forget, once the socket is
    // live.
    let _reassert = register::spawn(
        registration::service_manifest(endpoints, config.port),
        register::DEFAULT_REASSERT_INTERVAL,
    );

    axum::serve(listener, app).await.expect("server error");
}
