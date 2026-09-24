//! Background WAL checkpoint task — `mcp_service4agent::storage::Checkpointer`
//! on its own dedicated connection, driven by a timer, off the request path
//! and off [`crate::server::state::Storage::writer`]'s own `BatchWriter`
//! queue (see that type's own module doc for why a checkpoint must never
//! travel through it).
//!
//! Policy: PASSIVE every [`CHECKPOINT_INTERVAL`] — never waits on a reader or
//! the writer, so it can never itself cause a busy writer. Escalates to
//! TRUNCATE only when the on-disk `-wal` file is MEASURED (after the PASSIVE
//! pass) to still exceed [`WAL_TRUNCATE_THRESHOLD_BYTES`] — the same
//! escalation discipline `mcp_service4agent::storage::checkpoint`'s own
//! module doc prescribes. Motivation: the live `index.db-wal` grew to 761 MB
//! because nothing ever checkpointed it, so every connection open then had
//! to replay it from the HDD.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mcp_service4agent::storage::Checkpointer;

use crate::server::state::SharedState;

/// How often the routine PASSIVE checkpoint runs.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// On-disk `-wal` size, measured after a PASSIVE pass, past which the
/// routine timer escalates to TRUNCATE.
pub const WAL_TRUNCATE_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// One checkpoint attempt's outcome — cached by [`CheckpointCache`] for
/// `/health` and `mem_health`.
#[derive(Debug, Clone)]
pub struct CheckpointOutcome {
    pub mode: &'static str,
    pub ok: bool,
    pub wal_size_before_bytes: u64,
    pub wal_size_after_bytes: u64,
    pub error: Option<String>,
}

/// Last known checkpoint outcome, refreshed off the request path by
/// [`spawn_checkpoint_task`]. `/health`/`mem_health` read this; never the
/// database.
#[derive(Clone, Default)]
pub struct CheckpointCache {
    last: Option<(CheckpointOutcome, Instant)>,
}

impl CheckpointCache {
    /// Empty cache used before the first background checkpoint attempt
    /// completes — mirrors [`crate::server::stats::StatsCache::empty`].
    pub fn empty() -> Self {
        Self { last: None }
    }

    fn record(&mut self, outcome: CheckpointOutcome) {
        self.last = Some((outcome, Instant::now()));
    }

    /// The last outcome plus its age, or `None` before the first checkpoint
    /// attempt has run.
    pub fn snapshot(&self) -> Option<(CheckpointOutcome, Duration)> {
        self.last.as_ref().map(|(outcome, at)| (outcome.clone(), at.elapsed()))
    }
}

fn wal_file_size(wal_path: &Path) -> u64 {
    std::fs::metadata(wal_path).map(|m| m.len()).unwrap_or(0)
}

/// Runs one checkpoint pass: always PASSIVE first, escalating to TRUNCATE
/// only when the on-disk WAL, measured AFTER the PASSIVE pass, still exceeds
/// `truncate_threshold_bytes`. `truncate_threshold_bytes` is a parameter
/// (not hardcoded to [`WAL_TRUNCATE_THRESHOLD_BYTES`]) so a test can force
/// the escalation path against a small seeded WAL.
pub async fn run_checkpoint_once(checkpointer: &Checkpointer, wal_path: &Path, truncate_threshold_bytes: u64) -> CheckpointOutcome {
    let wal_size_before = wal_file_size(wal_path);

    let (busy, frames_in_log, frames_checkpointed) = match checkpointer.checkpoint_passive().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "wal passive checkpoint failed");
            return CheckpointOutcome {
                mode: "passive",
                ok: false,
                wal_size_before_bytes: wal_size_before,
                wal_size_after_bytes: wal_size_before,
                error: Some(e.to_string()),
            };
        }
    };
    let wal_size_after_passive = wal_file_size(wal_path);
    tracing::debug!(
        busy,
        frames_in_log,
        frames_checkpointed,
        wal_size_before_bytes = wal_size_before,
        wal_size_after_bytes = wal_size_after_passive,
        "passive wal checkpoint"
    );

    if wal_size_after_passive <= truncate_threshold_bytes {
        return CheckpointOutcome {
            mode: "passive",
            ok: true,
            wal_size_before_bytes: wal_size_before,
            wal_size_after_bytes: wal_size_after_passive,
            error: None,
        };
    }

    match checkpointer.checkpoint_truncate().await {
        Ok((busy, frames_in_log, frames_checkpointed)) => {
            let wal_size_after_truncate = wal_file_size(wal_path);
            tracing::info!(
                wal_size_before_bytes = wal_size_after_passive,
                wal_size_after_bytes = wal_size_after_truncate,
                busy,
                frames_in_log,
                frames_checkpointed,
                "wal truncate checkpoint"
            );
            CheckpointOutcome {
                mode: "truncate",
                ok: true,
                wal_size_before_bytes: wal_size_after_passive,
                wal_size_after_bytes: wal_size_after_truncate,
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, wal_size_bytes = wal_size_after_passive, "wal truncate checkpoint failed");
            CheckpointOutcome {
                mode: "truncate",
                ok: false,
                wal_size_before_bytes: wal_size_after_passive,
                wal_size_after_bytes: wal_size_after_passive,
                error: Some(e.to_string()),
            }
        }
    }
}

/// Spawns the background task that keeps `state.checkpoint_cache` fresh.
/// Opened by the caller AFTER the writer database is open (see
/// `bin/server.rs`) — `checkpointer` is its own dedicated connection,
/// entirely separate from `Storage::writer`/`Storage::reader_pool`.
pub fn spawn_checkpoint_task(state: SharedState, checkpointer: Arc<Checkpointer>, wal_path: PathBuf) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHECKPOINT_INTERVAL).await;
            let outcome = run_checkpoint_once(&checkpointer, &wal_path, WAL_TRUNCATE_THRESHOLD_BYTES).await;
            state
                .checkpoint_cache
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record(outcome);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn temp_path(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "m4a-checkpoint-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut s = path.as_os_str().to_os_string();
        s.push(suffix);
        PathBuf::from(s)
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sidecar(path, "-shm"));
    }

    /// A WAL measured above the threshold after the routine PASSIVE pass
    /// must be escalated to TRUNCATE and actually shrink on disk — the
    /// policy this module exists to implement (motivation: the live WAL
    /// grew to 761 MB because nothing ever checkpointed it).
    #[tokio::test]
    async fn a_wal_above_the_threshold_is_truncated() {
        let path = temp_path("truncate");

        // One plain writer connection seeds a WAL with real frames — same
        // seeding shape as `mcp_service4agent::storage::checkpoint`'s own
        // test.
        let mut writer = Connection::open(&path).expect("open writer");
        writer.pragma_update(None, "journal_mode", "WAL").expect("wal");
        writer.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);").expect("ddl");
        let tx = writer.transaction().expect("begin");
        for i in 0..2000 {
            tx.execute("INSERT INTO t (v) VALUES (?1)", [format!("row-{i}-{}", "x".repeat(200))])
                .expect("insert");
        }
        tx.commit().expect("commit");

        // Opened while `writer` is STILL alive: SQLite auto-checkpoints (and
        // truncates) a WAL-mode database's WAL when its LAST open connection
        // closes — dropping `writer` before a second connection exists would
        // reset the WAL out from under this test before it ever measures it.
        let checkpointer = Checkpointer::open(&path, -8192).expect("open checkpointer");

        let wal_path = sidecar(&path, "-wal");
        let wal_before = wal_file_size(&wal_path);
        assert!(wal_before > 0, "seed writes must leave real frames in the WAL");

        // Dropped now, before TRUNCATE runs, so TRUNCATE (which waits for
        // readers/the write lock) is never refused as busy.
        drop(writer);

        // Threshold far below the seeded WAL size forces escalation.
        let outcome = run_checkpoint_once(&checkpointer, &wal_path, 1024).await;
        assert_eq!(outcome.mode, "truncate", "a WAL over the threshold must escalate to TRUNCATE");
        assert!(outcome.ok, "TRUNCATE with no other connection open must not be refused as busy: {outcome:?}");
        assert!(
            outcome.wal_size_after_bytes < outcome.wal_size_before_bytes,
            "TRUNCATE must shrink the WAL ({} -> {})",
            outcome.wal_size_before_bytes,
            outcome.wal_size_after_bytes
        );

        drop(checkpointer);
        cleanup(&path);
    }

    /// The mirror case: a WAL measured at or under the threshold after
    /// PASSIVE must stay PASSIVE — TRUNCATE must never be the routine call
    /// (see this module's own doc, and
    /// `mcp_service4agent::storage::checkpoint`'s incident (b)).
    #[tokio::test]
    async fn a_wal_under_the_threshold_stays_passive() {
        let path = temp_path("passive-only");
        {
            let writer = Connection::open(&path).expect("open writer");
            writer.pragma_update(None, "journal_mode", "WAL").expect("wal");
            writer.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);").expect("ddl");
        }

        let wal_path = sidecar(&path, "-wal");
        let checkpointer = Checkpointer::open(&path, -8192).expect("open checkpointer");

        let outcome = run_checkpoint_once(&checkpointer, &wal_path, WAL_TRUNCATE_THRESHOLD_BYTES).await;
        assert_eq!(outcome.mode, "passive", "a tiny WAL must never escalate to TRUNCATE");
        assert!(outcome.ok);

        drop(checkpointer);
        cleanup(&path);
    }

    #[test]
    fn cache_reports_none_before_any_checkpoint_and_the_last_outcome_after() {
        let mut cache = CheckpointCache::empty();
        assert!(cache.snapshot().is_none(), "must report no outcome before the first checkpoint attempt");

        cache.record(CheckpointOutcome {
            mode: "passive",
            ok: true,
            wal_size_before_bytes: 100,
            wal_size_after_bytes: 10,
            error: None,
        });
        let (outcome, age) = cache.snapshot().expect("an outcome after record()");
        assert_eq!(outcome.mode, "passive");
        assert!(outcome.ok);
        assert!(age < Duration::from_secs(1));
    }
}
