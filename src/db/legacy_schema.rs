//! This crate's own legacy migration history, plus the one-time backfill
//! that lets the pre-rewrite 2.76 GB `./data/index.db` adopt
//! `mcp_service4agent::storage::MigrationRunner`'s versioned ledger without
//! replaying its own history. The versioned-migration ENGINE itself
//! (`Migration`/`MigrationRunner`) now comes from `mcp_service4agent::storage`
//! — this module keeps only what is specific to memory4agent: the SQL for
//! each historical schema version, and the backfill that inspects an
//! existing database to figure out which versions it already satisfies.

use rusqlite::Connection;
use tracing::info;

use mcp_service4agent::storage::Migration;

/// This crate's own migration history, mirroring the retired
/// `crate::db::schema`'s `SCHEMA_SQL` + `init_schema` `ALTER TABLE` chain
/// exactly — one [`Migration`] per historical schema change, in the same
/// order that swallow-errors chain applied them. Used both to build a fresh
/// database (every version's SQL actually runs) and, on a database
/// backfilled by [`seed_legacy_versions_if_needed`], as the version list
/// `MigrationRunner::run` finds already fully recorded (so none of this SQL
/// re-runs there).
///
/// Version → column/table each step adds, matching the table in
/// `docs/memory4agent/plans/2026-09-23-memory4agent-plan.md` piece 0.4:
///
/// | Version | Adds |
/// |---|---|
/// | 1 | base tables (workspaces, file_records, symbols, symbol_trigrams, call_edges, file_edges, crate_nodes, crate_dep_edges) |
/// | 2 | `symbols.body_hash` |
/// | 3 | `symbols.shape_fingerprint` + index |
/// | 4 | `symbols.shape_fingerprint_loose`, `field_count`, `shape_summary` + index |
/// | 5 | `symbols.callsite_count` |
/// | 6 | `symbols.type_use_count` + `type_use_edges` table + indexes |
/// | 7 | `symbols.impl_trait` + index |
/// | 8 | `idx_symbols_workspace_name` — composite `(workspace_id, name)` index; every `workspace_id = ? AND name = ?` read (the ambiguity-guard definition count, `get_symbols_by_name`, `resolve_symbol_to_crate`, `get_symbols_by_name_prefix`, `get_impls_of_trait`, …) used to seek `idx_symbols_name(name)` alone and then pay one rowid lookup per matching row just to filter `workspace_id` — 13.8s for a bare name with ~18k definitions against the live 2.76 GB DB, down to ~1s (covering-index scan, no rowid lookups) with this index (2026-09-24 read-path audit) |
pub fn legacy_schema_migrations() -> Vec<Migration> {
    vec![
        Migration::new(1, "base_tables", BASE_TABLES_SQL),
        Migration::new(2, "body_hash", "ALTER TABLE symbols ADD COLUMN body_hash TEXT;"),
        Migration::new(
            3,
            "shape_fingerprint",
            "ALTER TABLE symbols ADD COLUMN shape_fingerprint TEXT;
             CREATE INDEX IF NOT EXISTS idx_symbols_fingerprint ON symbols(shape_fingerprint);",
        ),
        Migration::new(
            4,
            "shape_fingerprint_loose",
            "ALTER TABLE symbols ADD COLUMN shape_fingerprint_loose TEXT;
             ALTER TABLE symbols ADD COLUMN field_count INTEGER;
             ALTER TABLE symbols ADD COLUMN shape_summary TEXT;
             CREATE INDEX IF NOT EXISTS idx_symbols_fingerprint_loose ON symbols(shape_fingerprint_loose);",
        ),
        Migration::new(
            5,
            "callsite_count",
            "ALTER TABLE symbols ADD COLUMN callsite_count INTEGER NOT NULL DEFAULT 0;",
        ),
        Migration::new(
            6,
            "type_use_edges",
            "ALTER TABLE symbols ADD COLUMN type_use_count INTEGER NOT NULL DEFAULT 0;
             CREATE TABLE IF NOT EXISTS type_use_edges (
                 workspace_id    TEXT NOT NULL,
                 from_symbol_id  INTEGER NOT NULL,
                 to_symbol_id    INTEGER NOT NULL,
                 use_kind        TEXT NOT NULL,
                 PRIMARY KEY (workspace_id, from_symbol_id, to_symbol_id, use_kind)
             );
             CREATE INDEX IF NOT EXISTS idx_type_use_from ON type_use_edges(workspace_id, from_symbol_id);
             CREATE INDEX IF NOT EXISTS idx_type_use_to ON type_use_edges(workspace_id, to_symbol_id);",
        ),
        Migration::new(
            7,
            "impl_trait",
            "ALTER TABLE symbols ADD COLUMN impl_trait TEXT;
             CREATE INDEX IF NOT EXISTS idx_symbols_impl_trait ON symbols(workspace_id, impl_trait) WHERE impl_trait IS NOT NULL;",
        ),
        Migration::new(
            8,
            "symbols_workspace_name_idx",
            "CREATE INDEX IF NOT EXISTS idx_symbols_workspace_name ON symbols(workspace_id, name);",
        ),
    ]
}

/// v1's shape — every table `crate::db::schema::SCHEMA_SQL` still creates
/// today, minus the columns/tables later versions add (those columns are
/// folded directly into `SCHEMA_SQL`'s `CREATE TABLE symbols` today, but
/// weren't when they first shipped — see the version table above).
const BASE_TABLES_SQL: &str = "
CREATE TABLE IF NOT EXISTS workspaces (
    id          TEXT PRIMARY KEY,
    root_path   TEXT NOT NULL,
    name        TEXT NOT NULL,
    indexed_at  INTEGER
);

CREATE TABLE IF NOT EXISTS file_records (
    workspace_id    TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    mtime           INTEGER NOT NULL,
    size            INTEGER NOT NULL DEFAULT 0,
    symbol_count    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, file_path)
);

CREATE TABLE IF NOT EXISTS symbols (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id    TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    name            TEXT NOT NULL,
    kind            TEXT NOT NULL,
    visibility      TEXT NOT NULL,
    line            INTEGER NOT NULL,
    col             INTEGER NOT NULL,
    parent_name     TEXT,
    signature       TEXT,
    FOREIGN KEY (workspace_id, file_path) REFERENCES file_records(workspace_id, file_path) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_symbols_workspace_file ON symbols(workspace_id, file_path);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);

CREATE TABLE IF NOT EXISTS symbol_trigrams (
    symbol_id   INTEGER NOT NULL,
    trigram     TEXT NOT NULL,
    PRIMARY KEY (symbol_id, trigram),
    FOREIGN KEY (symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_trigrams_trigram ON symbol_trigrams(trigram);

CREATE TABLE IF NOT EXISTS call_edges (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id        TEXT NOT NULL,
    caller_file         TEXT NOT NULL,
    caller_symbol_id    INTEGER NOT NULL DEFAULT 0,
    callee_name         TEXT NOT NULL,
    line                INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_call_edges_callee ON call_edges(callee_name, workspace_id);
CREATE INDEX IF NOT EXISTS idx_call_edges_caller_file ON call_edges(workspace_id, caller_file);

CREATE TABLE IF NOT EXISTS file_edges (
    workspace_id    TEXT NOT NULL,
    from_file       TEXT NOT NULL,
    to_file         TEXT NOT NULL,
    kind            TEXT NOT NULL,
    PRIMARY KEY (workspace_id, from_file, to_file, kind)
);

CREATE INDEX IF NOT EXISTS idx_file_edges_to ON file_edges(workspace_id, to_file);

CREATE TABLE IF NOT EXISTS crate_nodes (
    workspace_id        TEXT NOT NULL,
    name                TEXT NOT NULL,
    version             TEXT NOT NULL DEFAULT '',
    manifest_path       TEXT NOT NULL,
    source_dir          TEXT NOT NULL DEFAULT '',
    is_workspace_member INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (workspace_id, name)
);

CREATE TABLE IF NOT EXISTS crate_dep_edges (
    workspace_id    TEXT NOT NULL,
    from_crate      TEXT NOT NULL,
    to_crate        TEXT NOT NULL,
    is_path_dep     INTEGER NOT NULL DEFAULT 0,
    dep_path        TEXT,
    PRIMARY KEY (workspace_id, from_crate, to_crate)
);
";

/// One-time bootstrap for every `index.db` written before this rewrite
/// (including the live 2.76 GB `./data/index.db`). The old
/// `crate::db::schema::init_schema` applied schema changes via
/// `ALTER TABLE ...` with errors swallowed (`let _ =
/// conn.execute_batch(...)`) — replaying that same SQL under
/// `mcp_service4agent::storage::MigrationRunner` (which does NOT swallow
/// errors) would fail with "duplicate column name" on an already-migrated
/// DB. This function inspects `PRAGMA table_info(symbols)` + `sqlite_master`
/// and, if `schema_migrations` does not yet exist AND the `symbols` table
/// already does, back-fills `schema_migrations` rows for every legacy
/// version the existing schema already satisfies — WITHOUT re-running their
/// SQL — before `MigrationRunner::run` sees the ledger. No-op on a genuinely
/// fresh DB (`symbols` table absent — v1 creates it normally) and on a DB
/// that already has `schema_migrations` (already-seeded, or created fresh
/// post-rewrite).
pub fn seed_legacy_versions_if_needed(conn: &Connection) -> rusqlite::Result<()> {
    if table_exists(conn, "schema_migrations")? {
        return Ok(());
    }
    if !table_exists(conn, "symbols")? {
        // Genuinely fresh database — v1 creates `symbols` itself.
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version INTEGER PRIMARY KEY,
             label TEXT NOT NULL,
             applied_at TEXT NOT NULL DEFAULT (datetime('now'))
         );",
    )?;

    let mut satisfied: Vec<(u32, &str)> = vec![(1, "base_tables")];
    if column_exists(conn, "symbols", "body_hash")? {
        satisfied.push((2, "body_hash"));
    }
    if column_exists(conn, "symbols", "shape_fingerprint")? {
        satisfied.push((3, "shape_fingerprint"));
    }
    if column_exists(conn, "symbols", "shape_fingerprint_loose")? {
        satisfied.push((4, "shape_fingerprint_loose"));
    }
    if column_exists(conn, "symbols", "callsite_count")? {
        satisfied.push((5, "callsite_count"));
    }
    if column_exists(conn, "symbols", "type_use_count")? && table_exists(conn, "type_use_edges")? {
        satisfied.push((6, "type_use_edges"));
    }
    if column_exists(conn, "symbols", "impl_trait")? {
        satisfied.push((7, "impl_trait"));
    }

    for (version, label) in satisfied {
        conn.execute(
            "INSERT OR IGNORE INTO schema_migrations (version, label) VALUES (?1, ?2)",
            rusqlite::params![version, label],
        )?;
        info!(version, label, "backfilled legacy schema_migrations row");
    }

    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_service4agent::storage::MigrationRunner;

    fn mem() -> Connection {
        Connection::open_in_memory().expect("in-memory connection opens")
    }

    #[test]
    fn legacy_schema_migrations_build_a_fresh_db_from_scratch() {
        let mut conn = mem();
        MigrationRunner::new(legacy_schema_migrations())
            .run(&mut conn)
            .expect("v1..v8 apply cleanly against an empty database");

        let version_count: u32 = conn
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count query");
        assert_eq!(version_count, 8);

        // Every column each version was supposed to add is queryable.
        conn.execute(
            "INSERT INTO workspaces (id, root_path, name, indexed_at) VALUES ('ws', '/r', 'ws', NULL)",
            [],
        )
        .expect("insert workspace");
        conn.execute(
            "INSERT INTO file_records (workspace_id, file_path, mtime) VALUES ('ws', 'a.rs', 0)",
            [],
        )
        .expect("insert file_record");
        conn.execute(
            "INSERT INTO symbols (
                 workspace_id, file_path, name, kind, visibility, line, col,
                 body_hash, shape_fingerprint, shape_fingerprint_loose, field_count,
                 shape_summary, callsite_count, type_use_count, impl_trait
             ) VALUES ('ws', 'a.rs', 'Foo', 'struct', 'pub', 1, 0,
                 'h', 'fp', 'fpl', 2, 'summary', 0, 0, NULL)",
            [],
        )
        .expect("insert symbol touching every later-version column");
        conn.execute(
            "INSERT INTO type_use_edges (workspace_id, from_symbol_id, to_symbol_id, use_kind)
             VALUES ('ws', 1, 1, 'field')",
            [],
        )
        .expect("insert type_use_edges row");
    }

    /// Fixture snapshot of the exact legacy schema the live 2.76 GB
    /// `./data/index.db` carries today: `SCHEMA_SQL` as
    /// `crate::db::schema` currently writes it (already includes
    /// `body_hash` in `CREATE TABLE symbols`) followed by the same
    /// swallow-errors `ALTER TABLE` chain `init_schema` runs on every boot.
    /// Kept as a byte-for-byte copy here, not a call into `crate::db::schema`,
    /// so this test still proves the backfill against the pre-rewrite shape
    /// even after `crate::db::schema` itself is retired in a later piece.
    fn build_legacy_db() -> Connection {
        const LEGACY_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS workspaces (
    id          TEXT PRIMARY KEY,
    root_path   TEXT NOT NULL,
    name        TEXT NOT NULL,
    indexed_at  INTEGER
);

CREATE TABLE IF NOT EXISTS file_records (
    workspace_id    TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    mtime           INTEGER NOT NULL,
    size            INTEGER NOT NULL DEFAULT 0,
    symbol_count    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, file_path)
);

CREATE TABLE IF NOT EXISTS symbols (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id    TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    name            TEXT NOT NULL,
    kind            TEXT NOT NULL,
    visibility      TEXT NOT NULL,
    line            INTEGER NOT NULL,
    col             INTEGER NOT NULL,
    parent_name     TEXT,
    signature       TEXT,
    body_hash       TEXT,
    FOREIGN KEY (workspace_id, file_path) REFERENCES file_records(workspace_id, file_path) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_symbols_workspace_file ON symbols(workspace_id, file_path);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);

CREATE TABLE IF NOT EXISTS symbol_trigrams (
    symbol_id   INTEGER NOT NULL,
    trigram     TEXT NOT NULL,
    PRIMARY KEY (symbol_id, trigram),
    FOREIGN KEY (symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_trigrams_trigram ON symbol_trigrams(trigram);

CREATE TABLE IF NOT EXISTS call_edges (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id        TEXT NOT NULL,
    caller_file         TEXT NOT NULL,
    caller_symbol_id    INTEGER NOT NULL DEFAULT 0,
    callee_name         TEXT NOT NULL,
    line                INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_call_edges_callee ON call_edges(callee_name, workspace_id);
CREATE INDEX IF NOT EXISTS idx_call_edges_caller_file ON call_edges(workspace_id, caller_file);

CREATE TABLE IF NOT EXISTS file_edges (
    workspace_id    TEXT NOT NULL,
    from_file       TEXT NOT NULL,
    to_file         TEXT NOT NULL,
    kind            TEXT NOT NULL,
    PRIMARY KEY (workspace_id, from_file, to_file, kind)
);

CREATE INDEX IF NOT EXISTS idx_file_edges_to ON file_edges(workspace_id, to_file);

CREATE TABLE IF NOT EXISTS crate_nodes (
    workspace_id        TEXT NOT NULL,
    name                TEXT NOT NULL,
    version             TEXT NOT NULL DEFAULT '',
    manifest_path       TEXT NOT NULL,
    source_dir          TEXT NOT NULL DEFAULT '',
    is_workspace_member INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (workspace_id, name)
);

CREATE TABLE IF NOT EXISTS crate_dep_edges (
    workspace_id    TEXT NOT NULL,
    from_crate      TEXT NOT NULL,
    to_crate        TEXT NOT NULL,
    is_path_dep     INTEGER NOT NULL DEFAULT 0,
    dep_path        TEXT,
    PRIMARY KEY (workspace_id, from_crate, to_crate)
);
";
        let conn = mem();
        conn.execute_batch(LEGACY_SCHEMA_SQL).expect("legacy base schema applies");
        // Same swallow-errors ALTER chain `crate::db::schema::init_schema`
        // runs on every boot — `body_hash` duplicates the CREATE TABLE
        // column above and is expected to fail silently, exactly like the
        // real binary's behavior against a real legacy database.
        let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN body_hash TEXT;");
        let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN shape_fingerprint TEXT;");
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_symbols_fingerprint ON symbols(shape_fingerprint);",
        );
        let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN shape_fingerprint_loose TEXT;");
        let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN field_count INTEGER;");
        let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN shape_summary TEXT;");
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_symbols_fingerprint_loose ON symbols(shape_fingerprint_loose);",
        );
        let _ = conn.execute_batch(
            "ALTER TABLE symbols ADD COLUMN callsite_count INTEGER NOT NULL DEFAULT 0;",
        );
        let _ = conn.execute_batch(
            "ALTER TABLE symbols ADD COLUMN type_use_count INTEGER NOT NULL DEFAULT 0;",
        );
        let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN impl_trait TEXT;");
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_symbols_impl_trait ON symbols(workspace_id, impl_trait) WHERE impl_trait IS NOT NULL;",
        );
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS type_use_edges (
                workspace_id    TEXT NOT NULL,
                from_symbol_id  INTEGER NOT NULL,
                to_symbol_id    INTEGER NOT NULL,
                use_kind        TEXT NOT NULL,
                PRIMARY KEY (workspace_id, from_symbol_id, to_symbol_id, use_kind)
            );
            CREATE INDEX IF NOT EXISTS idx_type_use_from ON type_use_edges(workspace_id, from_symbol_id);
            CREATE INDEX IF NOT EXISTS idx_type_use_to ON type_use_edges(workspace_id, to_symbol_id);",
        );
        conn
    }

    #[test]
    fn seed_legacy_versions_then_run_matches_a_real_legacy_db() {
        let mut conn = build_legacy_db();

        seed_legacy_versions_if_needed(&conn).expect("backfill succeeds");
        MigrationRunner::new(legacy_schema_migrations())
            .run(&mut conn)
            .expect("v1..v7 don't re-run against an already-backfilled db; v8 runs for the first time");

        let rows: Vec<u32> = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query_map")
            .collect::<Result<_, _>>()
            .expect("collect");
        assert_eq!(rows, vec![1, 2, 3, 4, 5, 6, 7, 8]);

        // Every touched column still reads/writes fine after the backfill.
        conn.execute(
            "INSERT INTO workspaces (id, root_path, name, indexed_at) VALUES ('ws', '/r', 'ws', NULL)",
            [],
        )
        .expect("insert workspace");
        conn.execute(
            "INSERT INTO file_records (workspace_id, file_path, mtime) VALUES ('ws', 'a.rs', 0)",
            [],
        )
        .expect("insert file_record");
        conn.execute(
            "INSERT INTO symbols (
                 workspace_id, file_path, name, kind, visibility, line, col,
                 body_hash, shape_fingerprint, shape_fingerprint_loose, field_count,
                 shape_summary, callsite_count, type_use_count, impl_trait
             ) VALUES ('ws', 'a.rs', 'Foo', 'struct', 'pub', 1, 0,
                 'h', 'fp', 'fpl', 2, 'summary', 0, 0, 'MyTrait')",
            [],
        )
        .expect("insert symbol touching every legacy column");
        let impl_trait: Option<String> = conn
            .query_row("SELECT impl_trait FROM symbols WHERE name = 'Foo'", [], |r| r.get(0))
            .expect("read impl_trait back");
        assert_eq!(impl_trait.as_deref(), Some("MyTrait"));
        conn.execute(
            "INSERT INTO type_use_edges (workspace_id, from_symbol_id, to_symbol_id, use_kind)
             VALUES ('ws', 1, 1, 'field')",
            [],
        )
        .expect("insert type_use_edges row");
    }

    #[test]
    fn seed_is_a_no_op_on_a_genuinely_fresh_db() {
        let conn = mem();
        seed_legacy_versions_if_needed(&conn).expect("no-op on empty db");
        assert!(!table_exists(&conn, "schema_migrations").expect("check"));
    }

    #[test]
    fn seed_is_a_no_op_when_schema_migrations_already_exists() {
        let mut conn = mem();
        MigrationRunner::new(vec![Migration::new(1, "users", "CREATE TABLE users (id INTEGER);")])
            .run(&mut conn)
            .expect("seed schema_migrations via a normal migration run");
        seed_legacy_versions_if_needed(&conn).expect("no-op, ledger already exists");
        let count: u32 = conn
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count query");
        assert_eq!(count, 1);
    }
}
