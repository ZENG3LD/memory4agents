//! Phase 0.12 — in-memory read model (owner-approved 2026-09-24).
//!
//! Every incident on 2026-09-23/24 was SQL-on-HDD shaped: an N+1 in
//! `get_callers_of`, correlated per-symbol subqueries in the count
//! recompute, page-cache thrash on a 2.76 GB file on a SATA HDD, a reader
//! pool exhausted by runaway statements. The indexed corpus is small
//! (~500k symbols, a few million edges — hundreds of MB in memory). The hot
//! queries are graph walks and name lookups — memory, not SQL.
//!
//! SQLite stays the durable store; the indexer keeps writing through the
//! one `storage::BatchWriter`. Reads for the hot routes never touch disk:
//! [`snapshot::Snapshot`] is an immutable, per-workspace, in-memory copy of
//! the graph `crate::db::reader` otherwise re-derives from SQL on every
//! call; [`read_model::ReadModel`] holds one `Arc<Snapshot>` per workspace
//! id, swapped atomically after every completed index run.
//!
//! `arc-swap` is not in any nemo `Cargo.lock` today (checked across the
//! workspace before writing this), so `ReadModel` uses the plan's own
//! documented fallback — `std::sync::RwLock<Arc<..>>` per workspace,
//! swapped in one lock/insert/unlock — instead of adding a new dependency
//! for one atomic pointer swap.
//!
//! `crate::db::reader`'s SQL functions are NOT deleted: `bin/cli.rs` (the
//! `memory4agent` CLI binary) reads the SQLite index directly with no
//! server involved at all, and this module's own equivalence tests compare
//! every ported query against them on a fixture database.

pub mod read_model;
pub mod snapshot;

pub use read_model::ReadModel;
pub use snapshot::{Snapshot, SnapshotStats, MAX_CALLERS};

#[cfg(test)]
mod equivalence_tests;
