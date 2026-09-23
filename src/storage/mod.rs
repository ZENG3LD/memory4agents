//! Storage engine: a single writer connection (`Db` + `BatchWriter`) and a
//! read-only connection pool (`DbPool`), modeled on `servertoolkit-db` the
//! way `mail4agent-store-sqlite` modeled the same source — own types in
//! this crate, no `servertoolkit` dependency.
//!
//! `AppState`, every route, and the async `Indexer` are wired onto this
//! module (plan pieces 0.5-0.6): every HTTP/MCP read runs against
//! [`DbPool`], every write is enqueued on [`batch::BatchWriter`]. The
//! original mutex-per-connection storage this replaced (`crate::db::schema`
//! and a raw `Arc<Mutex<Connection>>` in `AppState`) has been retired —
//! `crate::db` now holds only `reader`/`writer`, pure functions over a
//! `&Connection`/`&mut Connection` regardless of how that connection was
//! obtained.

pub mod batch;
pub mod config;
pub mod db;
pub mod migrations;
pub mod pool;

pub use batch::{BatchConfig, BatchStats, BatchWriter, WriteOp};
pub use config::DbConfig;
pub use db::{Db, DbError};
pub use migrations::{Migration, MigrationRunner};
pub use pool::{DbConnection, DbPool, PoolStats};
