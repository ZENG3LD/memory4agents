//! [`Db`] — the sole writer connection handle: `Arc<tokio::sync::Mutex<Connection>>`
//! plus a label, WAL + foreign_keys pragmas, and versioned migrations.
//!
//! Modeled line-for-line on `servertoolkit-db/src/db.rs`, adapted the way
//! `mail4agent-store-sqlite/src/db.rs` adapted the same source: own types,
//! no `servertoolkit` dependency. Unlike stk's `Db` (which also exposes
//! async `read`/`write` helpers backed by the same single connection every
//! caller shares), this crate's `Db` is **never** handed to a route
//! handler directly — only [`crate::storage::batch::BatchWriter`] (its
//! single dedicated writer thread) and the migration bootstrap at boot
//! touch it. Every other reader goes through [`crate::storage::pool::DbPool`]
//! instead, which is why this type carries only the blocking helpers a
//! plain `std::thread` worker needs (`read_blocking`/`write_blocking`), not
//! `spawn_blocking`-wrapped async ones.

use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;
use tracing::info;

use crate::storage::config::DbConfig;
use crate::storage::migrations::MigrationRunner;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("open {path}: {source}")]
    Open { path: String, source: rusqlite::Error },
    #[error("pragma init: {0}")]
    Pragma(rusqlite::Error),
    #[error("migration: {0}")]
    Migration(rusqlite::Error),
    #[error("query: {0}")]
    Query(#[from] rusqlite::Error),
    #[error("worker panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("pool: {0}")]
    Pool(String),
}

/// Cheaply-clonable SQLite handle. Clones share the underlying `Mutex`, so a
/// single physical connection backs every clone — this crate's single
/// writer, per the module doc comment above.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
    label: Arc<str>,
}

impl Db {
    /// Opens the database and applies WAL (file-backed only) + foreign_keys
    /// pragmas. Does NOT run migrations — call [`Db::run_migrations`]
    /// separately once [`crate::storage::migrations::seed_legacy_versions_if_needed`]
    /// (if applicable) has had a chance to run against the same connection.
    pub fn open(cfg: &DbConfig) -> Result<Self, DbError> {
        let (conn, label) = match cfg {
            DbConfig::Path(path) => {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
                let conn = Connection::open(path).map_err(|e| DbError::Open {
                    path: path.display().to_string(),
                    source: e,
                })?;
                (conn, path.display().to_string())
            }
            DbConfig::InMemory => {
                let conn = Connection::open_in_memory().map_err(|e| DbError::Open {
                    path: ":memory:".into(),
                    source: e,
                })?;
                (conn, ":memory:".to_owned())
            }
        };

        if !cfg.is_in_memory() {
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(DbError::Pragma)?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(DbError::Pragma)?;
        // Same reasoning as `storage::pool::open_connection`'s own cache_size
        // bump: SQLite's ~2MB default page cache thrashes against a
        // multi-GB database on the writer's own bulk scans (e.g. the
        // post-index callsite_count/type_use_count recompute) just as
        // badly as it does on a reader's DISTINCT/JOIN.
        conn.pragma_update(None, "cache_size", -131_072i64)
            .map_err(DbError::Pragma)?;

        // `db::writer`'s hot indexing path (`insert_symbol`, `insert_symbol_trigrams`,
        // `insert_call_edge`, …) uses `Connection::prepare_cached` so a
        // statement compiled once is reused for the connection's whole
        // lifetime instead of being re-parsed by SQLite on every single
        // row — the dominant cost a plain `execute` call pays per row.
        // rusqlite's default capacity (16) already covers today's ~11
        // distinct hot statements; raised here so a later addition doesn't
        // silently start evicting and re-compiling.
        conn.set_prepared_statement_cache_capacity(64);

        info!(path = %label, "memory4agent db opened");
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
            label: Arc::from(label.as_str()),
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Run a set of [`crate::storage::migrations::Migration`]s against this
    /// db. Safe to call from inside a tokio runtime — uses `spawn_blocking`
    /// internally so the mutex is never held across an `.await`.
    pub async fn run_migrations(&self, runner: MigrationRunner) -> Result<(), DbError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            runner.run(&mut guard)
        })
        .await?
        .map_err(DbError::Migration)
    }

    /// Blocking read for plain sync threads (the batch writer's own worker
    /// thread, migration bootstrap before a tokio runtime exists, tests):
    /// waits on the mutex rather than requiring `spawn_blocking`. Calling
    /// this from inside an async task panics — see
    /// [`tokio::sync::Mutex::blocking_lock`]'s own contract.
    pub fn read_blocking<F, T>(&self, f: F) -> rusqlite::Result<T>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T>,
    {
        let guard = self.inner.blocking_lock();
        f(&guard)
    }

    /// Blocking write for plain sync threads — same discipline as
    /// [`Db::read_blocking`], but hands the closure a `&mut Connection` so
    /// it can open its own transaction.
    pub fn write_blocking<F, T>(&self, f: F) -> rusqlite::Result<T>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T>,
    {
        let mut guard = self.inner.blocking_lock();
        f(&mut guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::migrations::Migration;

    fn mem_db() -> Db {
        Db::open(&DbConfig::in_memory()).expect("in-memory db opens")
    }

    /// [`Db::read_blocking`]/[`Db::write_blocking`] wait on a
    /// `tokio::sync::Mutex` via `blocking_lock()`, which panics if called
    /// from a task already driven by a tokio runtime (see their own doc
    /// comments). A `#[tokio::test]` body IS such a task, so every test
    /// below that needs both `run_migrations` (async) and a blocking
    /// helper moves the blocking call onto `spawn_blocking` itself, the
    /// same way `BatchWriter::barrier_async` and every real async caller
    /// must.
    #[test]
    fn in_memory_open_sets_foreign_keys_on() {
        let db = mem_db();
        let fk: u32 = db
            .read_blocking(|c| c.query_row("PRAGMA foreign_keys;", [], |r| r.get(0)))
            .expect("reads pragma");
        assert_eq!(fk, 1);
        assert_eq!(db.label(), ":memory:");
    }

    #[tokio::test]
    async fn run_migrations_then_read_write_roundtrip() {
        let db = mem_db();
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
        )]))
        .await
        .expect("migration applies");

        let write_db = db.clone();
        tokio::task::spawn_blocking(move || {
            write_db.write_blocking(|c| {
                c.execute("INSERT INTO users (id, name) VALUES (1, 'alice')", [])?;
                c.execute("INSERT INTO users (id, name) VALUES (2, 'bob')", [])?;
                Ok(())
            })
        })
        .await
        .expect("join")
        .expect("writes succeed");

        let read_db = db.clone();
        let names: Vec<String> = tokio::task::spawn_blocking(move || {
            read_db.read_blocking(|c| {
                c.prepare("SELECT name FROM users ORDER BY id")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect()
            })
        })
        .await
        .expect("join")
        .expect("reads succeed");
        assert_eq!(names, vec!["alice".to_owned(), "bob".to_owned()]);
    }

    #[tokio::test]
    async fn file_open_creates_parent_dir() {
        let mut path = std::env::temp_dir();
        path.push(format!("m4a-storage-db-test-{}", std::process::id()));
        path.push("nested");
        path.push("dir");
        path.push("index.db");
        let cleanup_root = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .expect("test path has three ancestors")
            .to_path_buf();
        let _ = std::fs::remove_dir_all(&cleanup_root);

        let db = Db::open(&DbConfig::new(&path)).expect("opens, creating parent dirs");
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "init",
            "CREATE TABLE t (n INTEGER);",
        )]))
        .await
        .expect("migration applies");
        let write_db = db.clone();
        tokio::task::spawn_blocking(move || {
            write_db.write_blocking(|c| c.execute("INSERT INTO t VALUES (42)", []).map(|_| ()))
        })
        .await
        .expect("join")
        .expect("write succeeds");

        drop(db);
        let _ = std::fs::remove_dir_all(&cleanup_root);
    }
}
