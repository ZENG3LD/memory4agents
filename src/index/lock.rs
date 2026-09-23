//! Per-workspace single-flight lock for [`crate::index::indexer::Indexer::run`]
//! (and its `run_changed_paths` sibling) — shared between `POST /index` and
//! `crate::watch`'s debounced driver via `AppState::index_locks`.
//!
//! This closes a gap the `BatchWriter` alone does not: `BatchWriter`
//! serializes individual *ops* (one `SAVEPOINT` per `WriteOp`, one flushed
//! transaction per batch), not whole multi-phase `Indexer::run` calls. Two
//! concurrent runs for the SAME workspace could interleave their phases —
//! e.g. a `force=true` run's wipe (phase 3) landing between another run's
//! per-file upserts (phase 4), or one run's stale-file removal (phase 5)
//! racing the other's still-in-flight phase 4 — even though neither run's
//! individual writes are ever lost or corrupted at the SQL level. Holding
//! this lock for a run's full duration (its own `barrier_async` calls
//! included) makes that impossible: at most one `Indexer::run`/
//! `run_changed_paths` call per workspace id is ever in flight.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};

/// Registry of one `tokio::sync::Mutex<()>` per workspace id, created on
/// first use. Cheaply clonable (`Arc` internally) — every clone shares the
/// same underlying registry, which is what lets `AppState` hand the same
/// `IndexLocks` to both the `/index` route and `crate::watch`.
#[derive(Clone, Default)]
pub struct IndexLocks {
    inner: Arc<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>>,
}

impl IndexLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for exclusive access to `workspace_id`'s index lock. Different
    /// workspace ids never contend with each other — each gets its own
    /// entry in the registry, created lazily on first use and never
    /// removed (workspace ids are few and long-lived in practice, so this
    /// is not a meaningful leak).
    ///
    /// The caller MUST hold the returned guard for the full duration of the
    /// `Indexer::run`/`run_changed_paths` call it guards — dropping it
    /// early (or never acquiring it at all from some other call site)
    /// reopens the interleaving hole this type exists to close.
    pub async fn acquire(&self, workspace_id: &str) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut map = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            map.entry(workspace_id.to_string())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Proves the primitive the fix relies on: two concurrent acquires for
    /// the SAME workspace id never overlap. Deterministic (channel-signaled
    /// hold/release, no timing race to catch a violation) — mirrors
    /// `server::state::tests::slow_read_does_not_block_a_concurrent_read_or_write`'s
    /// own hold/signal/release pattern.
    #[tokio::test]
    async fn same_workspace_acquires_never_overlap() {
        let locks = IndexLocks::new();

        let (first_holding_tx, first_holding_rx) = tokio::sync::oneshot::channel();
        let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel::<()>();
        let locks_a = locks.clone();
        let first = tokio::spawn(async move {
            let _guard = locks_a.acquire("ws").await;
            let _ = first_holding_tx.send(());
            let _ = release_first_rx.await;
        });
        first_holding_rx.await.expect("first holder signals it is holding the lock");

        let (second_acquired_tx, second_acquired_rx) = tokio::sync::oneshot::channel();
        let locks_b = locks.clone();
        let second = tokio::spawn(async move {
            let _guard = locks_b.acquire("ws").await;
            let _ = second_acquired_tx.send(());
        });

        // The second acquire must NOT complete while the first is still
        // held — give it every chance to (wrongly) do so before asserting.
        let raced = tokio::time::timeout(Duration::from_millis(200), second_acquired_rx).await;
        assert!(
            raced.is_err(),
            "a second acquire for the same workspace completed while the first holder was still active"
        );

        let _ = release_first_tx.send(());
        first.await.expect("first holder task must not panic");
        second.await.expect("second holder task must not panic");
    }

    /// Distinguishes "per-workspace" from "one global lock for everything":
    /// two DIFFERENT workspace ids must be able to hold their locks at the
    /// same time.
    #[tokio::test]
    async fn different_workspaces_do_not_block_each_other() {
        let locks = IndexLocks::new();

        let (a_holding_tx, a_holding_rx) = tokio::sync::oneshot::channel();
        let (release_a_tx, release_a_rx) = tokio::sync::oneshot::channel::<()>();
        let locks_a = locks.clone();
        let task_a = tokio::spawn(async move {
            let _guard = locks_a.acquire("ws-a").await;
            let _ = a_holding_tx.send(());
            let _ = release_a_rx.await;
        });
        a_holding_rx.await.expect("workspace-a holder signals");

        // A different workspace id must acquire promptly even while
        // workspace-a's lock is held.
        let acquired_b = tokio::time::timeout(Duration::from_millis(200), locks.acquire("ws-b")).await;
        assert!(
            acquired_b.is_ok(),
            "a different workspace id must not wait on workspace-a's lock"
        );

        let _ = release_a_tx.send(());
        task_a.await.expect("workspace-a holder task must not panic");
    }
}
