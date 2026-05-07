use crate::error::CoreError;

pub const SCHEMA_SQL: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

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

/// Initialize the database schema (idempotent).
pub fn init_schema(conn: &rusqlite::Connection) -> Result<(), CoreError> {
    conn.execute_batch(SCHEMA_SQL)?;
    // Migration: add body_hash column if it doesn't exist yet (existing DBs).
    let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN body_hash TEXT;");
    // Migration: add shape_fingerprint column if it doesn't exist yet.
    let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN shape_fingerprint TEXT;");
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_symbols_fingerprint ON symbols(shape_fingerprint);",
    );
    // Migration v0.2.4: loose fingerprint, field count, shape summary.
    let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN shape_fingerprint_loose TEXT;");
    let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN field_count INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE symbols ADD COLUMN shape_summary TEXT;");
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_symbols_fingerprint_loose ON symbols(shape_fingerprint_loose);",
    );
    // Migration v0.2.5: callsite_count (updated at end of index pass).
    let _ = conn.execute_batch(
        "ALTER TABLE symbols ADD COLUMN callsite_count INTEGER NOT NULL DEFAULT 0;",
    );
    // Migration v0.2.6: type_use_count (in-degree from type_use_edges).
    let _ = conn.execute_batch(
        "ALTER TABLE symbols ADD COLUMN type_use_count INTEGER NOT NULL DEFAULT 0;",
    );
    // Migration v0.2.6: type_use_edges table.
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
    Ok(())
}
