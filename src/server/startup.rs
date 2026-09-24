//! Startup phase tracking (owner ruling, 2026-09-24): the listener binds
//! and starts answering `/health` before migrations or the read-model
//! snapshot build run — a slow migration (measured: a `CREATE INDEX` over
//! ~500k rows took 86s against the live HDD-backed database) must never
//! leave the port closed long enough for d-watchdog to declare the process
//! hung and kill it mid-migration, restarting the whole migration from
//! zero.
//!
//! Boot order (`bin/server.rs`): open the writer/reader-pool connections
//! (fast — pragmas only, no schema work) -> bind the listener -> start
//! `axum::serve` -> `tokio::spawn` [`run_migrations_and_build_snapshots`] to
//! run migrations, then build the initial read-model snapshot, then flip
//! [`StartupPhase::Ready`]. Every hot route gates on
//! [`StartupState::is_ready`] via `server::state::with_db` and
//! `server::state::snapshot`/`all_snapshots` — both refuse with a 503 and
//! the current phase until this completes. `/health` never gates on it —
//! it reports the phase, but always answers instantly (an `AtomicU8` load,
//! never a lock migrations or the snapshot build could be holding).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::server::state::SharedState;
use crate::storage::migrations::{seed_legacy_versions_if_needed, MigrationRunner};
use crate::storage::Db;
use crate::CoreError;

/// One phase in the boot sequence, in the order the server passes through
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupPhase {
    Migrating,
    BuildingSnapshot,
    Ready,
}

impl StartupPhase {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => StartupPhase::Migrating,
            1 => StartupPhase::BuildingSnapshot,
            _ => StartupPhase::Ready,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            StartupPhase::Migrating => 0,
            StartupPhase::BuildingSnapshot => 1,
            StartupPhase::Ready => 2,
        }
    }
}

impl std::fmt::Display for StartupPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StartupPhase::Migrating => "migrating",
            StartupPhase::BuildingSnapshot => "building_snapshot",
            StartupPhase::Ready => "ready",
        };
        write!(f, "{s}")
    }
}

/// Shared, lock-free startup phase. A plain `AtomicU8` rather than a
/// `RwLock<StartupPhase>` so `/health` (and every hot-route gate) reads it
/// with a single non-blocking load — never a lock a slow migration or
/// snapshot build could be holding.
#[derive(Clone)]
pub struct StartupState {
    phase: Arc<AtomicU8>,
}

impl StartupState {
    pub fn new() -> Self {
        Self { phase: Arc::new(AtomicU8::new(StartupPhase::Migrating.as_u8())) }
    }

    pub fn get(&self) -> StartupPhase {
        StartupPhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    pub fn set(&self, phase: StartupPhase) {
        self.phase.store(phase.as_u8(), Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.get() == StartupPhase::Ready
    }

    /// Convenience for tests (and any embedder that wants to skip the boot
    /// sequence entirely — e.g. a fixture database that's already fully
    /// migrated). Real server boot always starts at `Migrating`, via
    /// [`StartupState::new`].
    pub fn ready() -> Self {
        let state = Self::new();
        state.set(StartupPhase::Ready);
        state
    }
}

impl Default for StartupState {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs migrations, then builds the initial read-model snapshot for every
/// known workspace, then flips `state.startup` to [`StartupPhase::Ready`].
/// Intended to be `tokio::spawn`ed AFTER the listener is already bound and
/// serving `/health` — see `bin/server.rs`. A library function (not inlined
/// into the binary) specifically so it's unit-testable: see this module's
/// own tests for the "`/health` answers while a slow migration holds the
/// writer" proof.
pub async fn run_migrations_and_build_snapshots(
    state: SharedState,
    db: Db,
    migrations: MigrationRunner,
) -> Result<(), CoreError> {
    let seed_db = db.clone();
    tokio::task::spawn_blocking(move || seed_db.write_blocking(|conn| seed_legacy_versions_if_needed(conn)))
        .await??;
    db.run_migrations(migrations).await?;

    state.startup.set(StartupPhase::BuildingSnapshot);
    crate::model::read_model::build_all_workspaces(state.reader_pool(), &state.read_model).await?;

    state.startup.set(StartupPhase::Ready);
    tracing::info!("startup complete — serving hot routes from the read model");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::lock::IndexLocks;
    use crate::model::ReadModel;
    use crate::server::routes::health::health_snapshot;
    use crate::server::state::AppState;
    use crate::server::stats::StatsCache;
    use crate::storage::{BatchConfig, BatchWriter, DbConfig, DbPool, Migration};
    use std::sync::RwLock;
    use std::time::{Duration, Instant};

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "m4a-startup-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock after epoch").as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// The owner's own required proof: `/health` must answer instantly —
    /// and report the correct phase — while a slow migration still holds
    /// the writer's connection. The "slow migration" is simulated by a
    /// plain `db.write_blocking` call sleeping under the SAME
    /// `tokio::sync::Mutex<Connection>` `Db::run_migrations` itself locks
    /// (`storage::db::Db::run_migrations` takes that same mutex via
    /// `blocking_lock()`), so this is a faithful stand-in for a real slow
    /// `CREATE INDEX` without needing SQLite to actually run one for 86s.
    #[tokio::test]
    async fn health_answers_and_reports_phase_while_a_slow_migration_holds_the_writer() {
        let path = temp_db_path("slow-migration");
        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        let reader_pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("reader pool opens");
        let writer = BatchWriter::new(db.clone(), BatchConfig::default());

        let startup = StartupState::new();
        assert_eq!(startup.get(), StartupPhase::Migrating, "boot must start in the migrating phase");

        let state: SharedState = Arc::new(AppState {
            storage: crate::server::state::Storage::ready(writer, reader_pool),
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
            index_locks: IndexLocks::new(),
            last_index: Arc::new(RwLock::new(None)),
            read_model: ReadModel::new(),
            startup: startup.clone(),
        });

        // Hold the writer's own connection mutex on a background blocking
        // thread — the same mutex a real `MigrationRunner::run` locks.
        let (migration_started_tx, migration_started_rx) = tokio::sync::oneshot::channel();
        let hold_db = db.clone();
        let hold_task = tokio::task::spawn_blocking(move || {
            hold_db.write_blocking(|_conn| {
                let _ = migration_started_tx.send(());
                std::thread::sleep(Duration::from_millis(800));
                Ok(())
            })
        });
        migration_started_rx.await.expect("slow migration must signal it started");

        let started = Instant::now();
        let status = health_snapshot(&state);
        let elapsed = started.elapsed();

        assert!(status.ok, "cache-only /health must report ok before any refresh failure");
        assert_eq!(status.phase, "migrating", "phase must be reported while migrations are still running");
        assert!(
            elapsed < Duration::from_millis(300),
            "/health took {elapsed:?} while a slow migration held the writer — it must never wait on the database"
        );

        hold_task.await.expect("join").expect("hold task must not error");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// End-to-end: `run_migrations_and_build_snapshots` takes the phase
    /// from `Migrating` through `BuildingSnapshot` to `Ready`, and a
    /// workspace row present before the call is loaded into the read model
    /// by the time it returns.
    #[tokio::test]
    async fn run_migrations_and_build_snapshots_reaches_ready_and_loads_workspaces() {
        let path = temp_db_path("full-sequence");
        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        let reader_pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("reader pool opens");
        let writer = BatchWriter::new(db.clone(), BatchConfig::default());
        let startup = StartupState::new();

        let state: SharedState = Arc::new(AppState {
            storage: crate::server::state::Storage::ready(writer, reader_pool),
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
            index_locks: IndexLocks::new(),
            last_index: Arc::new(RwLock::new(None)),
            read_model: ReadModel::new(),
            startup: startup.clone(),
        });

        // Full base schema (so `Snapshot::build`'s `symbols`/`call_edges`/…
        // scans have real tables to query) plus one extra migration that
        // seeds a workspace row — by the time it runs, `workspaces` already
        // exists (created by `legacy_schema_migrations`'s own v1).
        let mut migration_list = crate::storage::migrations::legacy_schema_migrations();
        migration_list.push(Migration::new(
            9,
            "seed_workspace",
            "INSERT INTO workspaces (id, root_path, name, indexed_at) VALUES ('ws', '/r', 'ws', NULL);",
        ));
        let migrations = MigrationRunner::new(migration_list);

        run_migrations_and_build_snapshots(state.clone(), db.clone(), migrations)
            .await
            .expect("startup sequence succeeds");

        assert_eq!(startup.get(), StartupPhase::Ready);
        assert!(state.read_model.get("ws").is_some(), "workspace row present before the call must be loaded");

        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
