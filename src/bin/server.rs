use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use dig2memory::db::schema::init_schema;
use dig2memory::server::config::Config;
use dig2memory::server::state::{AppState, SharedState};
use dig2memory::server::stats::{spawn_stats_refresher, StatsCache};
use dig2memory::server::routes;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env();

    // Ensure data dir exists
    std::fs::create_dir_all(&config.data_dir).expect("failed to create data dir");

    let db_path = format!("{}/index.db", config.data_dir);
    let conn = rusqlite::Connection::open(&db_path).expect("failed to open SQLite");
    init_schema(&conn).expect("failed to init schema");

    let state: SharedState = Arc::new(AppState {
        db: Arc::new(Mutex::new(conn)),
        data_dir: config.data_dir,
        stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
    });

    spawn_stats_refresher(state.clone());

    let app = routes::api_router(state);

    let addr = format!("127.0.0.1:{}", config.port);
    tracing::info!("dig2memory-server listening on {}", addr);

    let listener = TcpListener::bind(&addr).await.expect("failed to bind");
    axum::serve(listener, app).await.expect("server error");
}
