//! `DbPool` — N reader connections over a `Vec<Connection>` + `Semaphore`.
//!
//! Copy of `servertoolkit-db/src/pool.rs`'s own reasoning, retargeted at
//! this crate's `storage::Db`: no `r2d2`/`deadpool` — both pin their own
//! `rusqlite`/`libsqlite3-sys`, which fights this crate's own `rusqlite =
//! "0.32"` at the `links = "sqlite3"` layer. ~150 LOC we own outright
//! instead.
//!
//! Internals:
//!   - `Vec<Connection>` of idle connections under a plain `std::sync::Mutex`
//!     (held for microseconds during push/pop, never across an `.await`).
//!   - `tokio::sync::Semaphore` with `max_size` permits gates checkout
//!     waiters (async-friendly, no thread parking).
//!   - `acquire().await` returns a [`DbConnection`] RAII guard; drop returns
//!     the connection to the pool and releases the permit.
//!
//! **Deliberate deviation from stk's pool**: every connection this pool
//! opens additionally runs `PRAGMA query_only = ON`, alongside
//! `journal_mode=WAL` / `foreign_keys=ON` / `busy_timeout=5000`. This pool
//! is read-only *by contract* — every HTTP/MCP read and heavy analytics
//! route runs against it, and the single writer lives in
//! [`crate::storage::batch::BatchWriter`] instead — so turning an accidental
//! write attempt on a pooled connection into a loud `SQLITE_READONLY` error
//! at the call site is strictly better than a silent write racing the
//! writer's own connection.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::info;

use crate::storage::config::DbConfig;
use crate::storage::db::DbError;

/// Reader-pool size default when a caller doesn't pick one explicitly.
/// `crate::server::config::Config::from_env`'s own default
/// (`MEMORY4AGENT_READER_POOL_SIZE`) is the one actually wired up at boot.
const DEFAULT_MAX_SIZE: usize = 8;

/// Read-only SQLite connection pool. Cheap to clone — `Arc<...>` internally.
#[derive(Clone)]
pub struct DbPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    cfg: DbConfig,
    label: Arc<str>,
    idle: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
    max_size: usize,
}

impl DbPool {
    /// Open the pool with a default size of 8 connections.
    pub fn open(cfg: &DbConfig) -> Result<Self, DbError> {
        Self::open_with_size(cfg, DEFAULT_MAX_SIZE)
    }

    /// Open the pool with `max_size` connections. Primes one connection
    /// eagerly (validates pragmas + file openability at boot rather than at
    /// first checkout); the rest are opened lazily on demand.
    pub fn open_with_size(cfg: &DbConfig, max_size: usize) -> Result<Self, DbError> {
        let max_size = max_size.max(1);
        let primer = open_connection(cfg)?;
        let label = match cfg {
            DbConfig::Path(p) => p.display().to_string(),
            DbConfig::InMemory => ":memory: (caution: per-connection isolation)".to_owned(),
        };
        info!(path = %label, size = max_size, "memory4agent reader pool opened");

        let inner = PoolInner {
            cfg: cfg.clone(),
            label: Arc::from(label.as_str()),
            idle: Mutex::new(vec![primer]),
            permits: Arc::new(Semaphore::new(max_size)),
            max_size,
        };
        Ok(Self { inner: Arc::new(inner) })
    }

    pub fn label(&self) -> &str {
        &self.inner.label
    }

    pub fn max_size(&self) -> usize {
        self.inner.max_size
    }

    /// Acquire a connection, awaiting if every slot is checked out.
    pub async fn acquire(&self) -> Result<DbConnection, DbError> {
        let permit = self
            .inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| DbError::Pool(format!("semaphore closed: {e}")))?;
        let conn_opt = {
            let mut idle = self.inner.idle.lock().expect("pool idle mutex poisoned");
            idle.pop()
        };
        let conn = if let Some(c) = conn_opt {
            c
        } else {
            let cfg = self.inner.cfg.clone();
            tokio::task::spawn_blocking(move || open_connection(&cfg)).await??
        };
        Ok(DbConnection {
            conn: Some(conn),
            pool: self.inner.clone(),
            _permit: permit,
        })
    }

    /// Async read — checks out a connection, runs `f` under `spawn_blocking`,
    /// returns the connection to the pool.
    pub async fn read<F, T>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let mut guard = self.acquire().await?;
        let conn = guard.take_for_blocking();
        let pool = self.inner.clone();
        let (result, conn) = tokio::task::spawn_blocking(move || {
            let r = f(&conn);
            (r, conn)
        })
        .await?;
        pool.return_connection(conn);
        drop(guard);
        result.map_err(DbError::Query)
    }

    /// Snapshot stats — current idle count + permit availability.
    pub fn stats(&self) -> PoolStats {
        let idle = self.inner.idle.lock().expect("idle mutex poisoned").len();
        PoolStats {
            max_size: self.inner.max_size,
            idle,
            available_permits: self.inner.permits.available_permits(),
        }
    }
}

impl PoolInner {
    fn return_connection(&self, conn: Connection) {
        let mut idle = self.idle.lock().expect("pool idle mutex poisoned");
        if idle.len() < self.max_size {
            idle.push(conn);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PoolStats {
    pub max_size: usize,
    pub idle: usize,
    pub available_permits: usize,
}

/// RAII guard returned by [`DbPool::acquire`]. Derefs to `Connection`. On
/// drop, returns the connection to the pool and releases the permit.
pub struct DbConnection {
    conn: Option<Connection>,
    pool: Arc<PoolInner>,
    _permit: OwnedSemaphorePermit,
}

impl DbConnection {
    /// Takes ownership of the underlying `Connection` for the `read` path's
    /// `spawn_blocking` move. Callers MUST return the connection via the
    /// pool themselves afterwards — direct consumer code should never call
    /// this; use [`DbPool::read`] or `Deref`/`DerefMut` instead.
    fn take_for_blocking(&mut self) -> Connection {
        self.conn.take().expect("connection already taken")
    }
}

impl std::ops::Deref for DbConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection taken without being returned")
    }
}

impl std::ops::DerefMut for DbConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("connection taken without being returned")
    }
}

impl Drop for DbConnection {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.pool.return_connection(c);
        }
    }
}

fn open_connection(cfg: &DbConfig) -> Result<Connection, DbError> {
    let (conn, label) = match cfg {
        DbConfig::Path(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            let c = Connection::open(path).map_err(|e| DbError::Open {
                path: path.display().to_string(),
                source: e,
            })?;
            (c, path.display().to_string())
        }
        DbConfig::InMemory => {
            let c = Connection::open_in_memory().map_err(|e| DbError::Open {
                path: ":memory:".into(),
                source: e,
            })?;
            (c, ":memory:".to_owned())
        }
    };

    if !cfg.is_in_memory() {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(DbError::Pragma)?;
    }
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(DbError::Pragma)?;
    conn.pragma_update(None, "busy_timeout", 5_000)
        .map_err(DbError::Pragma)?;
    // Read-only by contract — see the module doc comment's "Deliberate
    // deviation from stk's pool".
    conn.pragma_update(None, "query_only", "ON")
        .map_err(DbError::Pragma)?;

    tracing::debug!(path = %label, "memory4agent reader pool connection opened");
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Db;
    use crate::storage::migrations::{Migration, MigrationRunner};

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "m4a-storage-pool-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn opens_and_reads_migrated_schema() {
        let path = temp_path("migrate");
        let db = Db::open(&DbConfig::new(&path)).expect("writer opens");
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
        )]))
        .await
        .expect("migration applies");
        let write_db = db.clone();
        tokio::task::spawn_blocking(move || {
            write_db.write_blocking(|c| c.execute("INSERT INTO users (id, name) VALUES (1, 'alice')", []))
        })
        .await
        .expect("join")
        .expect("seed row");

        let pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("pool opens");
        let name: String = pool
            .read(|c| c.query_row("SELECT name FROM users WHERE id = 1", [], |r| r.get(0)))
            .await
            .expect("read succeeds");
        assert_eq!(name, "alice");

        drop(pool);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn pooled_connections_are_read_only_by_contract() {
        let path = temp_path("readonly");
        let db = Db::open(&DbConfig::new(&path)).expect("writer opens");
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "t",
            "CREATE TABLE t (n INTEGER);",
        )]))
        .await
        .expect("migration applies");

        let pool = DbPool::open_with_size(&DbConfig::new(&path), 2).expect("pool opens");
        let query_only: i64 = pool
            .read(|c| c.query_row("PRAGMA query_only;", [], |r| r.get(0)))
            .await
            .expect("read pragma");
        assert_eq!(query_only, 1);

        let write_result = pool.read(|c| c.execute("INSERT INTO t VALUES (1)", [])).await;
        assert!(
            matches!(write_result, Err(DbError::Query(rusqlite::Error::SqliteFailure(_, _)))),
            "expected a SQLITE_READONLY query error, got {write_result:?}"
        );

        drop(pool);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn max_size_caps_concurrent_acquires() {
        let path = temp_path("cap");
        let pool = DbPool::open_with_size(&DbConfig::new(&path), 2).expect("pool opens");
        let g1 = pool.acquire().await.expect("first acquire");
        let g2 = pool.acquire().await.expect("second acquire");

        let third = tokio::time::timeout(std::time::Duration::from_millis(120), pool.acquire()).await;
        assert!(third.is_err(), "third acquire should have timed out");

        drop(g1);
        let g3 = tokio::time::timeout(std::time::Duration::from_secs(1), pool.acquire())
            .await
            .expect("acquire completes after a slot frees up")
            .expect("acquire succeeds");
        drop(g2);
        drop(g3);
        drop(pool);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn stats_reflect_checkout_and_release() {
        let path = temp_path("stats");
        let pool = DbPool::open_with_size(&DbConfig::new(&path), 3).expect("pool opens");
        {
            let _g = pool.acquire().await.expect("acquire");
            let s = pool.stats();
            assert_eq!(s.max_size, 3);
            assert!(s.available_permits < s.max_size);
        }
        let s = pool.stats();
        assert_eq!(s.available_permits, s.max_size);
        drop(pool);
        let _ = std::fs::remove_file(&path);
    }
}
