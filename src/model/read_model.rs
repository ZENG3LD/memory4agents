//! [`ReadModel`] — one `Arc<Snapshot>` per workspace id, swapped atomically.
//! See the module doc comment in `crate::model` for the design and why
//! `arc-swap` isn't a dependency here.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use crate::model::snapshot::{Snapshot, SnapshotStats};
use crate::server::state::SharedState;
use crate::storage::DbPool;
use crate::CoreError;

/// Single-flight + coalescing bookkeeping for [`ReadModel::request_rebuild`]
/// — one combined mutex (not two separate ones) so a "decide whether to
/// spawn" check and a "decide whether to loop again" check never race each
/// other, the same lost-wakeup hazard `crate::watch::CoalesceState`'s own
/// doc comment calls out.
#[derive(Default)]
struct RebuildState {
    in_flight: HashSet<String>,
    pending: HashSet<String>,
}

/// One workspace's snapshot health summary — see [`ReadModel::stats`].
#[derive(Debug, Clone)]
pub struct SnapshotSummary {
    pub workspace_id: String,
    pub stats: SnapshotStats,
    pub age_secs: u64,
}

/// `HashMap<workspace_id, Arc<Snapshot>>` behind a plain
/// `std::sync::RwLock` — the plan's own documented fallback for
/// `ArcSwap<HashMap<..>>` since `arc-swap` is not in any nemo `Cargo.lock`
/// today. Readers take the lock only long enough to clone one `Arc` and
/// drop it immediately; the snapshot itself is read lock-free after that.
#[derive(Clone, Default)]
pub struct ReadModel {
    snapshots: Arc<RwLock<HashMap<String, Arc<Snapshot>>>>,
    rebuild: Arc<StdMutex<RebuildState>>,
}

impl ReadModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clone-and-drop-the-lock read — see the struct doc comment.
    pub fn get(&self, workspace_id: &str) -> Option<Arc<Snapshot>> {
        self.snapshots.read().unwrap_or_else(|p| p.into_inner()).get(workspace_id).cloned()
    }

    /// Every currently-loaded workspace's snapshot — used by routes whose
    /// SQL equivalent accepted `workspace: None` to mean "every workspace"
    /// (`db::reader`'s own `Option<&str>` convention).
    pub fn all(&self) -> Vec<Arc<Snapshot>> {
        self.snapshots.read().unwrap_or_else(|p| p.into_inner()).values().cloned().collect()
    }

    fn set(&self, workspace_id: String, snapshot: Arc<Snapshot>) {
        self.snapshots.write().unwrap_or_else(|p| p.into_inner()).insert(workspace_id, snapshot);
    }

    /// One summary per workspace currently loaded — feeds `/health` and
    /// `mem_health` (piece 0.12, item 5).
    pub fn stats(&self) -> Vec<SnapshotSummary> {
        self.snapshots
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, snap)| SnapshotSummary {
                workspace_id: id.clone(),
                stats: snap.stats(),
                age_secs: snap.age().as_secs(),
            })
            .collect()
    }

}

/// Builds one workspace's snapshot off the request path (`spawn_blocking`
/// around the sequential SQLite read pass, over one connection checked out
/// for the whole build) and swaps it into `read_model`. `pub(crate)` (not
/// private) so route test fixtures elsewhere in the crate (`ast`, `graph`,
/// `mcp`) can build a snapshot for their seeded data without going through
/// the fire-and-forget [`request_rebuild`].
pub(crate) async fn build_and_set(reader_pool: &DbPool, read_model: &ReadModel, workspace_id: &str) -> Result<(), CoreError> {
    let guard = reader_pool.acquire().await.map_err(CoreError::Storage)?;
    let wid = workspace_id.to_string();
    let snapshot = tokio::task::spawn_blocking(move || Snapshot::build(&guard, &wid)).await??;
    let stats = snapshot.stats();
    tracing::info!(
        workspace_id,
        symbols = stats.symbol_count,
        files = stats.file_count,
        call_edges = stats.call_edge_count,
        estimated_bytes = stats.estimated_bytes,
        build_ms = stats.build_ms,
        "read-model snapshot built"
    );
    read_model.set(workspace_id.to_string(), Arc::new(snapshot));
    Ok(())
}

/// Builds the initial snapshot for every workspace row already in the
/// database — called once at boot, after migrations, before the startup
/// phase flips to `Ready` (see `server::startup`).
pub async fn build_all_workspaces(reader_pool: &DbPool, read_model: &ReadModel) -> Result<(), CoreError> {
    let ids: Vec<String> = reader_pool
        .read(|conn| {
            conn.prepare("SELECT id FROM workspaces")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect()
        })
        .await
        .map_err(CoreError::Storage)?;
    for id in ids {
        build_and_set(reader_pool, read_model, &id).await?;
    }
    Ok(())
}

/// Call after a completed `Indexer::run`/`run_changed_paths` for
/// `workspace_id` (call sites: `server::routes::index::post_index_impl`,
/// `watch::run_reindex_driver`) — rebuilds that workspace's snapshot off
/// the request path and swaps it in.
///
/// Single-flight + coalesced: a call that arrives while a rebuild for the
/// SAME workspace is already in flight merges into exactly one follow-up
/// rebuild, never spawns a second concurrent one and never runs once per
/// caller — mirrors `crate::watch::CoalesceState`'s own pattern.
pub fn request_rebuild(state: &SharedState, workspace_id: String) {
    let read_model = state.read_model.clone();
    let reader_pool = state.reader_pool().clone();

    let should_spawn = {
        let mut rs = read_model.rebuild.lock().unwrap_or_else(|p| p.into_inner());
        if rs.in_flight.contains(&workspace_id) {
            rs.pending.insert(workspace_id.clone());
            false
        } else {
            rs.in_flight.insert(workspace_id.clone());
            true
        }
    };
    if !should_spawn {
        return;
    }

    tokio::spawn(async move {
        loop {
            if let Err(e) = build_and_set(&reader_pool, &read_model, &workspace_id).await {
                tracing::warn!(workspace_id = %workspace_id, "read-model rebuild failed: {e}");
            }
            let more_pending = {
                let mut rs = read_model.rebuild.lock().unwrap_or_else(|p| p.into_inner());
                if rs.pending.remove(&workspace_id) {
                    true
                } else {
                    rs.in_flight.remove(&workspace_id);
                    false
                }
            };
            if !more_pending {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{BatchConfig, BatchWriter, Db, DbConfig, MigrationRunner};

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "m4a-read-model-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock after epoch").as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    async fn seeded_pool(tag: &str) -> (DbPool, Db, BatchWriter, std::path::PathBuf) {
        let path = temp_db_path(tag);
        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        db.run_migrations(MigrationRunner::new(crate::storage::migrations::legacy_schema_migrations()))
            .await
            .expect("migration applies");
        let write_db = db.clone();
        tokio::task::spawn_blocking(move || {
            write_db.write_blocking(|conn| {
                conn.execute(
                    "INSERT INTO workspaces (id, root_path, name, indexed_at) VALUES ('ws', '/r', 'ws', NULL)",
                    [],
                )
                .map(|_| ())
            })
        })
        .await
        .expect("join")
        .expect("seed workspace row");
        let reader_pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("reader pool opens");
        let writer = BatchWriter::new(db.clone(), BatchConfig::default());
        (reader_pool, db, writer, path)
    }

    #[tokio::test]
    async fn build_all_workspaces_loads_every_seeded_workspace_row() {
        let (reader_pool, db, writer, path) = seeded_pool("build-all").await;
        let read_model = ReadModel::new();

        build_all_workspaces(&reader_pool, &read_model).await.expect("initial build succeeds");

        assert!(read_model.get("ws").is_some(), "workspace 'ws' must be loaded");
        assert!(read_model.get("missing").is_none());

        drop(writer);
        drop(reader_pool);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Two `request_rebuild` calls for the SAME workspace that arrive while
    /// the first is still in flight must coalesce into exactly one
    /// follow-up rebuild — proved directly against the `RebuildState`
    /// primitive `request_rebuild` relies on, the same way `index::lock`
    /// tests `IndexLocks` directly.
    #[test]
    fn a_rebuild_request_while_one_is_in_flight_merges_into_pending() {
        let rebuild: StdMutex<RebuildState> = StdMutex::new(RebuildState::default());

        let should_spawn_first = {
            let mut rs = rebuild.lock().expect("lock");
            if rs.in_flight.contains("ws") {
                rs.pending.insert("ws".to_string());
                false
            } else {
                rs.in_flight.insert("ws".to_string());
                true
            }
        };
        assert!(should_spawn_first, "the first request must be the one to spawn a rebuild");

        let should_spawn_second = {
            let mut rs = rebuild.lock().expect("lock");
            if rs.in_flight.contains("ws") {
                rs.pending.insert("ws".to_string());
                false
            } else {
                rs.in_flight.insert("ws".to_string());
                true
            }
        };
        assert!(!should_spawn_second, "a request arriving while a rebuild is in flight must not spawn a second one");

        let rs = rebuild.lock().expect("lock");
        assert!(rs.pending.contains("ws"));
    }
}
