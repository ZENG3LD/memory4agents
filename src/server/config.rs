/// Reader-pool size default when `MEMORY4AGENT_READER_POOL_SIZE` is unset or
/// unparsable. See `crate::storage::pool::DbPool`.
const DEFAULT_READER_POOL_SIZE: usize = 8;

pub struct Config {
    pub port: u16,
    pub data_dir: String,
    /// Number of concurrent read-only connections `crate::storage::DbPool`
    /// opens. Every HTTP/MCP read and heavy analytics route runs against
    /// this pool once `AppState` is rewired onto it (piece 0.5); it never
    /// touches the single writer connection `crate::storage::Db`/`BatchWriter`
    /// own.
    pub reader_pool_size: usize,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            port: std::env::var("MEMORY4AGENT_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(18200),
            data_dir: std::env::var("MEMORY4AGENT_DATA_DIR")
                .unwrap_or_else(|_| "./data".to_string()),
            reader_pool_size: std::env::var("MEMORY4AGENT_READER_POOL_SIZE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_READER_POOL_SIZE),
        }
    }
}
