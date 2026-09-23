//! Storage engine: a single writer connection (`Db` + `BatchWriter`) and a
//! read-only connection pool (`DbPool`), modeled on `servertoolkit-db` the
//! way `mail4agent-store-sqlite` modeled the same source — own types in
//! this crate, no `servertoolkit` dependency.
//!
//! This module is additive: `crate::db` (the mutex-per-connection storage
//! every existing route/indexer still uses) keeps compiling and running
//! unchanged until `AppState` and the indexer are rewired onto this module
//! (plan piece 0.5). Until then nothing in `bin/server.rs` constructs a
//! [`Db`]/[`DbPool`]/[`batch::BatchWriter`] — this module exists standalone,
//! covered by its own tests, so 0.5 rewires call sites onto an
//! already-proven storage engine instead of building and wiring it in the
//! same step.

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
