//! [`BatchWriter`] — copy of `servertoolkit-db/src/batch.rs`, retargeted at
//! this crate's own [`crate::storage::db::Db`].
//!
//! Formula: producers enqueue closures on an unbounded channel and never
//! wait on disk in their own tick; one dedicated worker thread drains the
//! queue and commits it as ONE transaction per batch — `max_ops` operations
//! or `flush_window` of silence, whichever comes first. Readers go through
//! [`crate::storage::pool::DbPool`] (WAL) — a write batch committing never
//! blocks them.
//!
//! - Every operation inside a batch runs under its own `SAVEPOINT`: one
//!   op's failure rolls back only that op, the batch continues (counted in
//!   [`BatchStats::failed`]).
//! - [`BatchWriter::barrier`] / [`BatchWriter::barrier_async`] is the
//!   durability point: returns once everything enqueued before it has
//!   committed (shutdown, snapshots, or — per the indexer's own boot
//!   sequence — a caller that must see its own write land before it
//!   returns).
//! - Queue order = DB order: one worker, FIFO.
//! - The queue is unbounded; [`BatchStats::queue_depth`] surfaces sustained
//!   growth as a signal to retune the window/batch size, not something this
//!   type blocks producers over.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tracing::warn;

use crate::storage::db::Db;

/// One write operation. Runs inside the worker's batch transaction under a
/// `SAVEPOINT`; opening a nested transaction of its own is not supported —
/// isolation for a single op is the `SAVEPOINT`, not a sub-transaction.
pub type WriteOp = Box<dyn FnOnce(&Connection) -> rusqlite::Result<()> + Send>;

/// Batch tuning: the ceiling on operations per commit and the quiet window
/// before flushing a partial batch.
#[derive(Clone, Copy, Debug)]
pub struct BatchConfig {
    /// Maximum operations committed in one transaction.
    pub max_ops: usize,
    /// How long to wait for the queue to grow before committing what's
    /// there.
    pub flush_window: Duration,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_ops: 256,
            flush_window: Duration::from_millis(50),
        }
    }
}

/// Snapshot of the worker's counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct BatchStats {
    pub enqueued: u64,
    pub committed: u64,
    pub failed: u64,
    pub batches: u64,
    pub last_batch_ops: u64,
    pub last_batch_dur: Duration,
}

impl BatchStats {
    /// Operations currently waiting in the queue (approximate under a race).
    pub fn queue_depth(&self) -> u64 {
        self.enqueued.saturating_sub(self.committed + self.failed)
    }
}

#[derive(Default, Debug)]
struct StatsInner {
    enqueued: AtomicU64,
    committed: AtomicU64,
    failed: AtomicU64,
    batches: AtomicU64,
    last_batch_ops: AtomicU64,
    last_batch_micros: AtomicU64,
}

enum Cmd {
    Op(WriteOp),
    Barrier(mpsc::Sender<()>),
}

/// Cheaply-clonable handle onto the write queue. Dropping the last handle
/// stops the worker: the channel closes, whatever is queued flushes, then
/// the thread exits.
#[derive(Clone)]
pub struct BatchWriter {
    tx: mpsc::Sender<Cmd>,
    stats: Arc<StatsInner>,
}

impl BatchWriter {
    /// Starts the worker over `db` (the same physical connection every
    /// clone of `db` shares — the worker holds its mutex only for the
    /// duration of a batch commit).
    pub fn new(db: Db, cfg: BatchConfig) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let stats = Arc::new(StatsInner::default());
        let worker_stats = stats.clone();
        let label = db.label().to_string();
        std::thread::Builder::new()
            .name(format!("m4a-batch-{label}"))
            .spawn(move || run(db, cfg, rx, worker_stats))
            .expect("batch writer thread spawn");
        Self { tx, stats }
    }

    /// Enqueues an operation. Fire-and-forget: never blocks the caller.
    pub fn send(&self, op: WriteOp) {
        self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        if self.tx.send(Cmd::Op(op)).is_err() {
            self.stats.failed.fetch_add(1, Ordering::Relaxed);
            warn!("batch writer worker is gone — op dropped");
        }
    }

    /// Durability point: blocks the calling thread until every op enqueued
    /// before this call has committed. Returns `false` if the worker is
    /// gone or doesn't respond within 30s.
    pub fn barrier(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Cmd::Barrier(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(Duration::from_secs(30)).is_ok()
    }

    /// Async wrapper over [`Self::barrier`] for tokio call sites.
    pub async fn barrier_async(&self) -> bool {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.barrier()).await.unwrap_or(false)
    }

    pub fn stats(&self) -> BatchStats {
        BatchStats {
            enqueued: self.stats.enqueued.load(Ordering::Relaxed),
            committed: self.stats.committed.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            batches: self.stats.batches.load(Ordering::Relaxed),
            last_batch_ops: self.stats.last_batch_ops.load(Ordering::Relaxed),
            last_batch_dur: Duration::from_micros(self.stats.last_batch_micros.load(Ordering::Relaxed)),
        }
    }
}

fn run(db: Db, cfg: BatchConfig, rx: mpsc::Receiver<Cmd>, stats: Arc<StatsInner>) {
    let mut ops: Vec<WriteOp> = Vec::with_capacity(cfg.max_ops.min(1024));
    let mut barriers: Vec<mpsc::Sender<()>> = Vec::new();
    'outer: while let Ok(first) = rx.recv() {
        match first {
            Cmd::Op(op) => ops.push(op),
            Cmd::Barrier(b) => {
                barriers.push(b);
                flush(&db, &mut ops, &mut barriers, &stats);
            }
        }
        loop {
            if ops.len() >= cfg.max_ops {
                flush(&db, &mut ops, &mut barriers, &stats);
            }
            match rx.recv_timeout(cfg.flush_window) {
                Ok(Cmd::Op(op)) => ops.push(op),
                Ok(Cmd::Barrier(b)) => {
                    barriers.push(b);
                    flush(&db, &mut ops, &mut barriers, &stats);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    flush(&db, &mut ops, &mut barriers, &stats);
                    break 'outer;
                }
            }
        }
        flush(&db, &mut ops, &mut barriers, &stats);
    }
}

/// One transaction per batch; each op runs under its own `SAVEPOINT` so a
/// single op's failure is isolated.
fn flush(db: &Db, ops: &mut Vec<WriteOp>, barriers: &mut Vec<mpsc::Sender<()>>, stats: &StatsInner) {
    if !ops.is_empty() {
        let started = Instant::now();
        let n = ops.len() as u64;
        let mut committed = 0u64;
        let mut failed = 0u64;
        let res = db.write_blocking(|conn| {
            let tx = conn.transaction()?;
            for op in ops.drain(..) {
                tx.execute_batch("SAVEPOINT batch_op")?;
                match op(&tx) {
                    Ok(()) => {
                        tx.execute_batch("RELEASE batch_op")?;
                        committed += 1;
                    }
                    Err(e) => {
                        tx.execute_batch("ROLLBACK TO batch_op")?;
                        tx.execute_batch("RELEASE batch_op")?;
                        failed += 1;
                        warn!(error = %e, "batch op failed — rolled back, batch continues");
                    }
                }
            }
            tx.commit()
        });
        match res {
            Ok(()) => {
                stats.committed.fetch_add(committed, Ordering::Relaxed);
                stats.failed.fetch_add(failed, Ordering::Relaxed);
                stats.batches.fetch_add(1, Ordering::Relaxed);
                stats.last_batch_ops.store(n, Ordering::Relaxed);
                stats
                    .last_batch_micros
                    .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                stats.failed.fetch_add(n, Ordering::Relaxed);
                warn!(error = %e, "batch transaction failed — whole batch dropped");
            }
        }
    }
    for b in barriers.drain(..) {
        let _ = b.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::config::DbConfig;
    use crate::storage::migrations::{Migration, MigrationRunner};

    fn file_db(name: &str) -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("m4a-batch-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("batch.db");
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&DbConfig::new(&path)).expect("db opens");
        (db, dir)
    }

    /// Runs `read_blocking` via `spawn_blocking`: called from inside
    /// `#[tokio::test]` bodies, where a direct `blocking_lock()` call would
    /// panic (see `storage::db::tests`' own note on this).
    async fn count_rows(db: &Db) -> i64 {
        let db = db.clone();
        tokio::task::spawn_blocking(move || {
            db.read_blocking(|c| c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)))
        })
        .await
        .expect("join")
        .expect("count query")
    }

    #[tokio::test]
    async fn batch_commits_all_ops() {
        let (db, dir) = file_db("all");
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "t",
            "CREATE TABLE t (n INTEGER PRIMARY KEY);",
        )]))
        .await
        .expect("migration applies");
        let w = BatchWriter::new(db.clone(), BatchConfig::default());
        for i in 0..10 {
            w.send(Box::new(move |c| {
                c.execute("INSERT INTO t VALUES (?1)", [i])?;
                Ok(())
            }));
        }
        assert!(w.barrier_async().await);
        assert_eq!(count_rows(&db).await, 10);
        let s = w.stats();
        assert_eq!(s.committed, 10);
        assert!(s.batches >= 1);
        assert_eq!(s.queue_depth(), 0);
        drop(w);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn batch_failing_op_isolated_by_savepoint() {
        let (db, dir) = file_db("iso");
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "t",
            "CREATE TABLE t (n INTEGER PRIMARY KEY);",
        )]))
        .await
        .expect("migration applies");
        let w = BatchWriter::new(
            db.clone(),
            BatchConfig { max_ops: 256, flush_window: Duration::from_millis(200) },
        );
        w.send(Box::new(|c| {
            c.execute("INSERT INTO t VALUES (1)", [])?;
            Ok(())
        }));
        w.send(Box::new(|c| {
            c.execute("INSERT INTO t VALUES (1)", [])?; // duplicate PK — fails
            Ok(())
        }));
        w.send(Box::new(|c| {
            c.execute("INSERT INTO t VALUES (2)", [])?;
            Ok(())
        }));
        assert!(w.barrier_async().await);
        assert_eq!(count_rows(&db).await, 2);
        let s = w.stats();
        assert_eq!(s.committed, 2);
        assert_eq!(s.failed, 1);
        drop(w);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn batch_barrier_async_works() {
        let (db, dir) = file_db("async");
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "t",
            "CREATE TABLE t (n INTEGER PRIMARY KEY);",
        )]))
        .await
        .expect("migration applies");
        let w = BatchWriter::new(db.clone(), BatchConfig::default());
        w.send(Box::new(|c| {
            c.execute("INSERT INTO t VALUES (7)", [])?;
            Ok(())
        }));
        assert!(w.barrier_async().await);
        assert_eq!(count_rows(&db).await, 1);
        drop(w);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn dropping_last_writer_handle_stops_the_worker_cleanly() {
        let (db, dir) = file_db("drop");
        db.run_migrations(MigrationRunner::new(vec![Migration::new(
            1,
            "t",
            "CREATE TABLE t (n INTEGER PRIMARY KEY);",
        )]))
        .await
        .expect("migration applies");
        let w = BatchWriter::new(db.clone(), BatchConfig::default());
        w.send(Box::new(|c| {
            c.execute("INSERT INTO t VALUES (1)", [])?;
            Ok(())
        }));
        assert!(w.barrier_async().await);
        drop(w);
        // Worker thread has exited; the row it committed is still there.
        assert_eq!(count_rows(&db).await, 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
