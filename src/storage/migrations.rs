//! Versioned schema migrations — copy of `servertoolkit-db/src/migrations.rs`
//! retargeted at this crate's own `rusqlite::Connection`.
//!
//! Each [`Migration`] is a `(version, label, sql)` triple. Versions must be
//! strictly increasing in the slice handed to [`MigrationRunner::new`]. The
//! runner keeps a `schema_migrations(version, label, applied_at)` table
//! tracking which versions have been applied; re-running is idempotent —
//! unlike the retired `crate::db::schema::init_schema`, which re-applied its
//! whole `ALTER TABLE` chain on every boot and depended on swallowing
//! "duplicate column" errors to survive that.
//!
//! This crate's own migration history (mirroring `crate::db::schema`'s
//! version-by-version shape) plus the one-time legacy-database backfill
//! that lets an existing `index.db` adopt this ledger without re-running
//! already-applied SQL land in a follow-up piece.

use rusqlite::Connection;
use tracing::info;

#[derive(Debug, Clone)]
pub struct Migration {
    pub version: u32,
    pub label: String,
    pub sql: String,
}

impl Migration {
    pub fn new(version: u32, label: impl Into<String>, sql: impl Into<String>) -> Self {
        Self {
            version,
            label: label.into(),
            sql: sql.into(),
        }
    }
}

pub struct MigrationRunner {
    migrations: Vec<Migration>,
}

impl MigrationRunner {
    /// Create a runner. Migrations must be supplied in **strictly
    /// increasing** version order — the constructor panics otherwise so the
    /// error is loud and at boot time, not at runtime.
    pub fn new(migrations: Vec<Migration>) -> Self {
        for w in migrations.windows(2) {
            assert!(
                w[0].version < w[1].version,
                "migrations must be in strictly increasing version order: \
                 v{} ({:?}) >= v{} ({:?})",
                w[0].version,
                w[0].label,
                w[1].version,
                w[1].label,
            );
        }
        Self { migrations }
    }

    /// Apply all pending migrations against the connection inside one
    /// `BEGIN EXCLUSIVE` transaction, tracked in
    /// `schema_migrations(version, label, applied_at)`. `busy_timeout` is
    /// bumped to 30s for the duration of the exclusive lock so a second
    /// instance starting concurrently waits rather than failing outright,
    /// then restored to whatever it was before.
    pub fn run(self, conn: &mut Connection) -> rusqlite::Result<()> {
        let prev_busy_ms: i64 = conn
            .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
            .unwrap_or(0);
        conn.pragma_update(None, "busy_timeout", 30_000)?;

        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?;

            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                     version INTEGER PRIMARY KEY,
                     label TEXT NOT NULL,
                     applied_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );",
            )?;

            for m in self.migrations {
                let already: Option<u32> = tx
                    .query_row(
                        "SELECT version FROM schema_migrations WHERE version = ?1",
                        [m.version],
                        |r| r.get(0),
                    )
                    .ok();
                if already.is_some() {
                    continue;
                }
                tx.execute_batch(&m.sql)?;
                tx.execute(
                    "INSERT INTO schema_migrations (version, label) VALUES (?1, ?2)",
                    rusqlite::params![m.version, m.label],
                )?;
                info!(version = m.version, label = %m.label, "applied migration");
            }

            tx.commit()?;
            Ok(())
        })();

        let _ = conn.pragma_update(None, "busy_timeout", prev_busy_ms);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_in_memory().expect("in-memory connection opens")
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn out_of_order_panics() {
        let _ = MigrationRunner::new(vec![
            Migration::new(2, "b", "SELECT 1;"),
            Migration::new(1, "a", "SELECT 1;"),
        ]);
    }

    #[test]
    fn applies_migrations_once() {
        let mut conn = mem();
        let runner = MigrationRunner::new(vec![
            Migration::new(1, "users", "CREATE TABLE users (id INTEGER PRIMARY KEY);"),
            Migration::new(2, "add_name", "ALTER TABLE users ADD COLUMN name TEXT NOT NULL DEFAULT '';"),
        ]);
        runner.run(&mut conn).expect("first run applies both");

        let rows: Vec<(u32, String)> = conn
            .prepare("SELECT version, label FROM schema_migrations ORDER BY version")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query_map")
            .collect::<Result<_, _>>()
            .expect("collect");
        assert_eq!(rows, vec![(1, "users".into()), (2, "add_name".into())]);

        conn.execute("INSERT INTO users (id, name) VALUES (1, 'alice')", [])
            .expect("insert against migrated schema");

        // Re-running the same runner is a no-op (idempotent).
        let runner = MigrationRunner::new(vec![
            Migration::new(1, "users", "SELECT 1;"),
            Migration::new(2, "add_name", "SELECT 1;"),
        ]);
        runner.run(&mut conn).expect("second run is a no-op");

        let count: u32 = conn
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count query");
        assert_eq!(count, 2);
    }
}
