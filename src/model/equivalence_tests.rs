//! Equivalence tests (piece 0.12): for one fixture database, every ported
//! `Snapshot` query must return the same data the retired SQL path
//! (`db::reader`, `search::trigram::fuzzy_search`) returned for the exact
//! same inputs. `db::reader`'s functions are not deleted by this piece —
//! `bin/cli.rs` still calls them directly against the SQLite file with no
//! server involved — so this module compares against them directly rather
//! than against a frozen expectation.
//!
//! `Symbol`/`TypeUser`/`CrateNode`/`DepEdge` don't derive `PartialEq`
//! (not needed anywhere else in the crate), so comparisons below go through
//! `serde_json::to_value` — two symbols with the same field VALUES compare
//! equal regardless of how each side constructed them. Several SQL
//! functions being mirrored have no `ORDER BY` at all (`get_deps_of_file`,
//! `get_reverse_file_deps`, `list_crate_nodes`/`list_crate_deps`,
//! `get_callers_of`, `resolve_symbol_to_crate`, `get_symbols_by_name`) —
//! those comparisons normalize order (sort, or compare as a set) on BOTH
//! sides before asserting equality, since SQLite's own row order for an
//! unordered `SELECT` is an implementation detail, not a contract.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::db::legacy_schema::legacy_schema_migrations;
use crate::db::{reader, writer};
use crate::model::snapshot::Snapshot;
use crate::search::trigram::fuzzy_search;
use mcp_service4agent::storage::MigrationRunner;
use crate::types::{
    CallEdge, CrateNode, DepEdge, FileEdge, FileEdgeKind, FileRecord, Symbol, SymbolKind, Visibility,
};

const WS: &str = "ws";

fn symbol(file_path: &str, name: &str, kind: SymbolKind, line: u32, parent_name: Option<&str>, impl_trait: Option<&str>) -> Symbol {
    Symbol {
        id: 0,
        workspace_id: WS.to_string(),
        file_path: file_path.to_string(),
        name: name.to_string(),
        kind,
        visibility: Visibility::Public,
        line,
        col: 0,
        parent_name: parent_name.map(str::to_string),
        signature: Some(format!("fn {name}()")),
        body_hash: None,
        shape_fingerprint: None,
        shape_fingerprint_loose: None,
        field_count: None,
        shape_summary: None,
        callsite_count: 0,
        type_use_count: 0,
        impl_trait: impl_trait.map(str::to_string),
    }
}

/// Seeds one small, deterministic fixture covering every ported query:
/// struct/trait/impl symbols (both trait and inherent impls, so
/// `impls_of_trait`/`inherent_impls_of_type` both have real data), a
/// precise + a legacy (`caller_symbol_id = 0`) call edge, file edges in
/// both directions with NON-tied hotspot counts (`c.rs` has 2 incoming
/// edges, `b.rs` has 1 — avoids an SQL-vs-snapshot tie-break mismatch on a
/// property neither side's `ORDER BY` actually pins down), a type-use edge,
/// and a two-crate dependency graph.
fn seed_fixture(conn: &Connection) -> rusqlite::Result<HashMap<&'static str, i64>> {
    for file in ["a.rs", "b.rs", "c.rs"] {
        writer::upsert_file_record(conn, &FileRecord { workspace_id: WS.to_string(), file_path: file.to_string(), mtime: 0, size: 0, symbol_count: 0 })?;
    }

    let mut ids: HashMap<&'static str, i64> = HashMap::new();
    macro_rules! insert {
        ($key:literal, $sym:expr) => {{
            let sym = $sym;
            let name = sym.name.clone();
            let id = writer::insert_symbol(conn, &sym)?;
            // Populates `symbol_trigrams` — the SQL `fuzzy_search` path
            // reads from this table, not from `symbols` directly.
            writer::insert_symbol_trigrams(conn, id, &name)?;
            ids.insert($key, id);
        }};
    }
    insert!("Foo_struct", symbol("a.rs", "Foo", SymbolKind::Struct, 1, None, None));
    insert!("Bar_trait", symbol("b.rs", "Bar", SymbolKind::Trait, 1, None, None));
    insert!("Foo_impl_Bar", symbol("a.rs", "Foo", SymbolKind::Impl, 5, None, Some("Bar")));
    insert!("Foo_impl_inherent", symbol("a.rs", "Foo", SymbolKind::Impl, 10, None, None));
    insert!("Foo_compute_method", symbol("a.rs", "compute", SymbolKind::Method, 11, Some("Foo"), None));
    insert!("helper_fn", symbol("c.rs", "helper", SymbolKind::Function, 1, None, None));
    insert!("caller_fn", symbol("b.rs", "caller_fn", SymbolKind::Function, 2, None, None));

    // Precise call edge: caller_fn -> helper, resolved caller_symbol_id.
    writer::insert_call_edge(
        conn,
        &CallEdge { workspace_id: WS.to_string(), caller_file: "b.rs".to_string(), caller_symbol_id: ids["caller_fn"], callee_name: "helper".to_string(), line: 2 },
    )?;
    // Legacy call edge: an unresolved call site in c.rs (caller_symbol_id
    // = 0) calling a name nothing defines — `get_callers_of` must fall
    // back to "the first symbol in c.rs by line", which is `helper` itself.
    writer::insert_call_edge(
        conn,
        &CallEdge { workspace_id: WS.to_string(), caller_file: "c.rs".to_string(), caller_symbol_id: 0, callee_name: "orphan_callee".to_string(), line: 3 },
    )?;

    // File edges: a.rs -> b.rs, a.rs -> c.rs, b.rs -> c.rs — c.rs ends up
    // with in-degree 2, b.rs with in-degree 1 (no tie).
    writer::upsert_file_edge(conn, &FileEdge { workspace_id: WS.to_string(), from_file: "a.rs".to_string(), to_file: "b.rs".to_string(), kind: FileEdgeKind::UseDecl })?;
    writer::upsert_file_edge(conn, &FileEdge { workspace_id: WS.to_string(), from_file: "a.rs".to_string(), to_file: "c.rs".to_string(), kind: FileEdgeKind::ModDecl })?;
    writer::upsert_file_edge(conn, &FileEdge { workspace_id: WS.to_string(), from_file: "b.rs".to_string(), to_file: "c.rs".to_string(), kind: FileEdgeKind::UseDecl })?;

    // Type-use edge: caller_fn uses Foo (the struct) as a field type.
    writer::insert_type_use_edge(
        conn,
        &crate::types::TypeUseEdge { workspace_id: WS.to_string(), from_symbol_id: ids["caller_fn"], to_symbol_id: ids["Foo_struct"], use_kind: "field" },
    )?;

    writer::upsert_crate_node(conn, &CrateNode { workspace_id: WS.to_string(), name: "core".to_string(), version: "0.1.0".to_string(), manifest_path: "core/Cargo.toml".to_string(), source_dir: "core".to_string(), is_workspace_member: true })?;
    writer::upsert_crate_node(conn, &CrateNode { workspace_id: WS.to_string(), name: "utils".to_string(), version: "0.1.0".to_string(), manifest_path: "utils/Cargo.toml".to_string(), source_dir: "utils".to_string(), is_workspace_member: true })?;
    writer::upsert_crate_dep_edge(conn, &DepEdge { workspace_id: WS.to_string(), from_crate: "core".to_string(), to_crate: "utils".to_string(), is_path_dep: true, dep_path: Some("../utils".to_string()) })?;

    Ok(ids)
}

fn json(v: &impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(v).expect("value serializes")
}

fn json_set(v: &[impl serde::Serialize]) -> HashSet<String> {
    v.iter().map(|x| json(x).to_string()).collect()
}

async fn fixture() -> (Connection, Snapshot, HashMap<&'static str, i64>) {
    let mut conn = Connection::open_in_memory().expect("in-memory connection opens");
    MigrationRunner::new(legacy_schema_migrations()).run(&mut conn).expect("migrations apply");
    let ids = seed_fixture(&conn).expect("fixture seeds");
    let snapshot = Snapshot::build(&conn, WS).expect("snapshot builds");
    (conn, snapshot, ids)
}

#[tokio::test]
async fn fuzzy_search_matches_the_sql_trigram_path() {
    let (conn, snap, _ids) = fixture().await;

    let mut from_snapshot = snap.fuzzy_search("Foo", 50, 0.1);
    let mut from_sql = fuzzy_search(&conn, Some(WS), "Foo", 50, 0.1).expect("sql fuzzy_search succeeds");

    let key = |h: &crate::types::SearchHit| (h.symbol.file_path.clone(), h.symbol.line);
    from_snapshot.sort_by_key(key);
    from_sql.sort_by_key(key);

    assert_eq!(from_snapshot.len(), from_sql.len(), "hit count must match");
    for (a, b) in from_snapshot.iter().zip(from_sql.iter()) {
        assert_eq!(json(&a.symbol), json(&b.symbol));
        assert!((a.score - b.score).abs() < 1e-6, "scores must match: {} vs {}", a.score, b.score);
    }
}

#[tokio::test]
async fn symbols_in_file_matches_get_symbols_in_file() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot = snap.symbols_in_file("a.rs");
    let from_sql = reader::get_symbols_in_file(&conn, WS, "a.rs").expect("sql succeeds");
    assert_eq!(json(&from_snapshot), json(&from_sql), "ORDER BY line is significant here — direct comparison");
}

#[tokio::test]
async fn count_definitions_matches_count_symbol_definitions() {
    let (conn, snap, _ids) = fixture().await;
    assert_eq!(snap.count_definitions("Foo"), reader::count_symbol_definitions(&conn, "Foo", Some(WS)).expect("sql succeeds"));
    assert_eq!(snap.count_definitions("does-not-exist"), reader::count_symbol_definitions(&conn, "does-not-exist", Some(WS)).expect("sql succeeds"));
}

#[tokio::test]
async fn callers_of_matches_get_callers_of_including_the_legacy_fallback() {
    let (conn, snap, _ids) = fixture().await;

    for callee in ["helper", "orphan_callee", "nobody_calls_this"] {
        let mut from_snapshot = snap.callers_of(callee);
        let mut from_sql = reader::get_callers_of(&conn, callee, Some(WS)).expect("sql succeeds");
        from_snapshot.sort_by_key(|s| s.id);
        from_sql.sort_by_key(|s| s.id);
        assert_eq!(json(&from_snapshot), json(&from_sql), "callers of {callee:?} must match (set-compared — SQL has no ORDER BY here)");
    }
}

#[tokio::test]
async fn deps_of_file_matches_get_deps_of_file() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot = json_set(&snap.deps_of_file("a.rs"));
    let from_sql = json_set(&reader::get_deps_of_file(&conn, WS, "a.rs").expect("sql succeeds"));
    assert_eq!(from_snapshot, from_sql);
}

#[tokio::test]
async fn reverse_deps_of_file_matches_get_reverse_file_deps() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot: HashSet<String> = snap.reverse_deps_of_file("c.rs").into_iter().collect();
    let from_sql: HashSet<String> = reader::get_reverse_file_deps(&conn, WS, "c.rs").expect("sql succeeds").into_iter().collect();
    assert_eq!(from_snapshot, from_sql);
    assert!(from_snapshot.contains("a.rs") && from_snapshot.contains("b.rs"));
}

#[tokio::test]
async fn crate_graph_matches_list_crate_nodes_and_deps() {
    let (conn, snap, _ids) = fixture().await;
    let nodes_snapshot = json_set(snap.crate_nodes());
    let nodes_sql = json_set(&reader::list_crate_nodes(&conn, Some(WS)).expect("sql succeeds"));
    assert_eq!(nodes_snapshot, nodes_sql);

    let edges_snapshot = json_set(snap.crate_dep_edges());
    let edges_sql = json_set(&reader::list_crate_deps(&conn, Some(WS)).expect("sql succeeds"));
    assert_eq!(edges_snapshot, edges_sql);
}

#[tokio::test]
async fn hotspots_matches_get_hotspots() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot: HashMap<String, u32> = snap.hotspots(100).into_iter().collect();
    let from_sql: HashMap<String, u32> = reader::get_hotspots(&conn, Some(WS), 100).expect("sql succeeds").into_iter().collect();
    assert_eq!(from_snapshot, from_sql);
    assert_eq!(from_snapshot.get("c.rs"), Some(&2), "c.rs has 2 incoming file edges by fixture design");
    assert_eq!(from_snapshot.get("b.rs"), Some(&1), "b.rs has 1 incoming file edge by fixture design");
}

#[tokio::test]
async fn resolve_symbol_to_crate_matches_the_sql_path() {
    let (conn, _snap, _ids) = fixture().await;
    // `resolve_symbol_to_crate` matches by file_path prefix against
    // `crate_nodes.source_dir` — give Foo's struct definition a SECOND
    // file record under `core/` and repoint just that one symbol at it (the
    // base fixture's files sit at the workspace root, matching no crate's
    // `source_dir`); every other symbol keeps referencing the original
    // `a.rs` row, so its FK stays satisfied throughout.
    conn.execute(
        "INSERT INTO file_records (workspace_id, file_path, mtime, size, symbol_count) VALUES ('ws', 'core/a.rs', 0, 0, 0)",
        [],
    )
    .expect("insert core/a.rs file record");
    conn.execute("UPDATE symbols SET file_path = 'core/a.rs' WHERE name = 'Foo' AND kind = 'struct'", []).expect("update symbol path");
    let snap = Snapshot::build(&conn, WS).expect("snapshot rebuilds after the path update");

    let from_snapshot = snap.resolve_symbol_to_crate("Foo");
    let from_sql = reader::resolve_symbol_to_crate(&conn, WS, "Foo").expect("sql succeeds");

    let key = |pairs: &[(Symbol, CrateNode)]| -> HashSet<String> {
        pairs.iter().map(|(s, c)| format!("{}|{}", json(s), json(c))).collect()
    };
    assert_eq!(key(&from_snapshot), key(&from_sql));
    assert!(!from_snapshot.is_empty(), "Foo under core/ must resolve to the 'core' crate on both paths");
}

#[tokio::test]
async fn symbols_by_name_matches_get_symbols_by_name() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot = json_set(&snap.symbols_by_name("Foo", None));
    let from_sql = json_set(&reader::get_symbols_by_name(&conn, "Foo", Some(WS), None).expect("sql succeeds"));
    assert_eq!(from_snapshot, from_sql);
    assert_eq!(from_snapshot.len(), 3, "struct + trait-impl + inherent-impl all share the name 'Foo'");
}

#[tokio::test]
async fn symbols_by_name_prefix_matches_get_symbols_by_name_prefix() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot = snap.symbols_by_name_prefix("Foo", None);
    let from_sql = reader::get_symbols_by_name_prefix(&conn, WS, "Foo", None).expect("sql succeeds");
    assert_eq!(json(&from_snapshot), json(&from_sql), "ORDER BY file_path, line is significant here");
}

#[tokio::test]
async fn type_users_matches_get_type_users() {
    let (conn, snap, ids) = fixture().await;
    let foo_id = ids["Foo_struct"];
    let from_snapshot = snap.type_users(foo_id);
    let from_sql = reader::get_type_users(&conn, WS, foo_id).expect("sql succeeds");

    assert_eq!(from_snapshot.len(), from_sql.len());
    for (a, b) in from_snapshot.iter().zip(from_sql.iter()) {
        assert_eq!(a.from_symbol_id, b.from_symbol_id);
        assert_eq!(a.name, b.name);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.file_path, b.file_path);
        assert_eq!(a.line, b.line);
        assert_eq!(a.use_kind, b.use_kind);
    }
    assert_eq!(from_snapshot.len(), 1, "exactly one type-use edge was seeded, from caller_fn");
}

#[tokio::test]
async fn impls_of_trait_matches_get_impls_of_trait() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot = snap.impls_of_trait("Bar", None);
    let from_sql = reader::get_impls_of_trait(&conn, WS, "Bar", None).expect("sql succeeds");
    assert_eq!(json(&from_snapshot), json(&from_sql));
    assert_eq!(from_snapshot.len(), 1);
}

#[tokio::test]
async fn inherent_impls_of_type_matches_get_inherent_impls_of_type() {
    let (conn, snap, _ids) = fixture().await;
    let from_snapshot = snap.inherent_impls_of_type("Foo", None);
    let from_sql = reader::get_inherent_impls_of_type(&conn, WS, "Foo", None).expect("sql succeeds");
    assert_eq!(json(&from_snapshot), json(&from_sql));
    assert_eq!(from_snapshot.len(), 1, "only the impl_trait IS NULL block for Foo counts");
}

#[tokio::test]
async fn method_count_for_impl_counts_the_seeded_method() {
    let (_conn, snap, _ids) = fixture().await;
    assert_eq!(snap.method_count_for_impl("a.rs", "Foo"), 1, "exactly one method (compute) has parent_name = Foo in a.rs");
    assert_eq!(snap.method_count_for_impl("a.rs", "NoSuchType"), 0);
}
