use cargo_metadata::MetadataCommand;
use crate::error::CoreError;
use crate::index::indexer::is_excluded;
use crate::types::{CrateNode, DepEdge};
use std::path::Path;
use walkdir::WalkDir;

/// Discovers every submodule-rooted `Cargo.toml` directly under
/// `workspace_root` (one level deep, filtered through the same
/// [`is_excluded`] predicate `index::indexer`'s own workspace walk uses —
/// target/node_modules/.cargo/.git/build/hidden dirs), runs `cargo metadata
/// --no-deps` per manifest found (a submodule with its own internal cargo
/// workspace, e.g. a multi-crate layout, yields every member package from
/// ONE such call), and merges every submodule's nodes/edges into one graph
/// for `workspace_id`.
///
/// `nemo` itself has no root `Cargo.toml` — it is a directory of
/// independent git submodules, each its own Cargo workspace — so there is
/// no synthetic "workspace root" node here, only the union of what every
/// submodule manifest actually reports. Cross-submodule path dependencies
/// (e.g. one submodule depending on a sibling via `{ path = "../other" }`)
/// resolve naturally because edges are keyed by crate NAME and every
/// submodule contributes nodes to the same accumulator.
///
/// A per-manifest failure (a submodule whose `cargo metadata` invocation
/// errors) is logged and that submodule is skipped, not propagated as a
/// whole-call error — matches `index::indexer::collect_source_files`'s own
/// per-entry error handling, so one broken submodule manifest never blanks
/// out the rest of the graph. The caller ([`crate::index::indexer::Indexer`])
/// treats the result as the authoritative current graph and prunes any
/// previously stored crate node/edge not present in it — see
/// `crate::db::writer::delete_stale_crate_nodes`/`delete_stale_crate_dep_edges`.
pub fn parse_multi_manifest_workspace(
    workspace_root: &Path,
    workspace_id: &str,
) -> Result<(Vec<CrateNode>, Vec<DepEdge>), CoreError> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for entry in WalkDir::new(workspace_root)
        .min_depth(1)
        .max_depth(1)
        .into_iter()
        .filter_entry(|e| !is_excluded(e))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("multi-manifest workspace scan error: {e}");
                continue;
            }
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("Cargo.toml");
        if !manifest_path.is_file() {
            continue;
        }
        match parse_one_manifest(&manifest_path, workspace_root, workspace_id) {
            Ok((mut n, mut e)) => {
                nodes.append(&mut n);
                edges.append(&mut e);
            }
            Err(e) => tracing::warn!(
                "manifest parse error at {}: {e}",
                manifest_path.display()
            ),
        }
    }

    Ok((nodes, edges))
}

/// Runs `cargo metadata --no-deps` against one submodule's manifest,
/// returning its workspace members' nodes and their declared dependency
/// edges. Every path is made relative to `strip_root` — the OVERALL
/// multi-manifest workspace root (e.g. `nemo/`), not the submodule's own
/// root — so `source_dir`/`manifest_path`/`dep_path` stay consistent across
/// every submodule's contribution to the same accumulator.
fn parse_one_manifest(
    manifest_path: &Path,
    strip_root: &Path,
    workspace_id: &str,
) -> Result<(Vec<CrateNode>, Vec<DepEdge>), CoreError> {
    // Run `cargo metadata` ourselves (hidden window, see `crate::proc`) and
    // only let `cargo_metadata` parse its output: `MetadataCommand::exec`
    // spawns a visible console window per submodule on Windows.
    let manifest_err = |reason: String| CoreError::Manifest {
        path: manifest_path.to_string_lossy().replace('\\', "/"),
        reason,
    };
    let output = crate::proc::hidden("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--manifest-path"])
        .arg(manifest_path)
        .output()
        .map_err(|e| manifest_err(e.to_string()))?;
    if !output.status.success() {
        return Err(manifest_err(String::from_utf8_lossy(&output.stderr).trim().to_string()));
    }
    let metadata = MetadataCommand::parse(String::from_utf8_lossy(&output.stdout))
        .map_err(|e| manifest_err(e.to_string()))?;

    let root_str = strip_root.to_string_lossy().replace('\\', "/");
    // Ensure root_str has no trailing slash for consistent prefix stripping.
    let root_str = root_str.trim_end_matches('/');

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for pkg in &metadata.packages {
        let pkg_manifest = pkg.manifest_path.to_string().replace('\\', "/");
        // source_dir = directory of the manifest, relative to workspace root.
        let pkg_dir = pkg_manifest
            .strip_suffix("/Cargo.toml")
            .unwrap_or(&pkg_manifest);
        let source_dir = if let Some(stripped) = pkg_dir.strip_prefix(root_str) {
            stripped.trim_start_matches('/').to_string()
        } else {
            pkg_dir.to_string()
        };

        // manifest_path stored relative, e.g. "dig2memory/Cargo.toml" or
        // "mail4agent/crates/app/Cargo.toml" for a multi-crate submodule.
        let manifest_rel = if let Some(stripped) = pkg_manifest.strip_prefix(root_str) {
            stripped.trim_start_matches('/').to_string()
        } else {
            pkg_manifest.clone()
        };

        let is_workspace_member = metadata.workspace_members.contains(&pkg.id);

        nodes.push(CrateNode {
            workspace_id: workspace_id.to_string(),
            name: pkg.name.clone(),
            version: pkg.version.to_string(),
            manifest_path: manifest_rel,
            source_dir,
            is_workspace_member,
        });

        // Emit dependency edges for all declared deps.
        for dep in &pkg.dependencies {
            let dep_path = dep
                .path
                .as_ref()
                .map(|p| p.to_string().replace('\\', "/"));

            // Make dep_path relative to workspace root when possible.
            let dep_path_rel = dep_path.map(|p| {
                if let Some(stripped) = p.strip_prefix(root_str) {
                    stripped.trim_start_matches('/').to_string()
                } else {
                    p
                }
            });

            edges.push(DepEdge {
                workspace_id: workspace_id.to_string(),
                from_crate: pkg.name.clone(),
                to_crate: dep.name.clone(),
                is_path_dep: dep_path_rel.is_some(),
                dep_path: dep_path_rel,
            });
        }
    }

    Ok((nodes, edges))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{reader, writer};
    use rusqlite::Connection;
    use std::collections::HashSet;

    fn write_manifest(dir: &Path, name: &str, deps: &[(&str, &str)]) {
        std::fs::create_dir_all(dir.join("src")).expect("create crate src dir");
        std::fs::write(dir.join("src/lib.rs"), b"pub fn marker() {}\n")
            .expect("write crate lib.rs");

        let mut manifest = format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        );
        if !deps.is_empty() {
            manifest.push_str("\n[dependencies]\n");
            for (dep_name, rel_path) in deps {
                manifest.push_str(&format!("{dep_name} = {{ path = \"{rel_path}\" }}\n"));
            }
        }
        std::fs::write(dir.join("Cargo.toml"), manifest).expect("write crate manifest");
    }

    /// Reproduces the recon's concrete bug: `crate_nodes`/`crate_dep_edges`
    /// seeded with the 7 dead rows a long-gone root `Cargo.toml` left
    /// behind (`nemo`, `nemo-cookie`, `nemo-db`, `nemo-ml`, `nemo-research`,
    /// `nemo-viz`, `trading-connectors-v4`), then proves a
    /// `parse_multi_manifest_workspace` run over a small synthetic
    /// multi-submodule fixture — followed by the same
    /// prune-then-upsert sequence `Indexer::run`'s Phase 6 performs — makes
    /// every stale row disappear while the fixture's real crates land.
    #[test]
    fn stale_root_manifest_rows_are_pruned_and_real_submodule_crates_land() {
        let root = std::env::temp_dir().join(format!(
            "m4a-crates-multi-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create fixture root");

        // Two independent submodule-rooted manifests, mirroring nemo's own
        // "directory of subgits, no root Cargo.toml" layout. `crate-b`
        // depends on `crate-a` via a path dep, so a real dependency edge
        // exists to prove real edges survive the prune.
        write_manifest(&root.join("crate-a"), "m4a-fixture-crate-a", &[]);
        write_manifest(
            &root.join("crate-b"),
            "m4a-fixture-crate-b",
            &[("m4a-fixture-crate-a", "../crate-a")],
        );

        let (nodes, edges) = parse_multi_manifest_workspace(&root, "ws")
            .expect("multi-manifest parse never returns Err for a readable root");

        let node_names: HashSet<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(node_names.contains("m4a-fixture-crate-a"));
        assert!(node_names.contains("m4a-fixture-crate-b"));
        assert!(
            edges
                .iter()
                .any(|e| e.from_crate == "m4a-fixture-crate-b"
                    && e.to_crate == "m4a-fixture-crate-a"
                    && e.is_path_dep),
            "expected a path-dep edge crate-b -> crate-a, got {edges:?}"
        );

        let mut conn = Connection::open_in_memory().expect("in-memory connection opens");
        mcp_service4agent::storage::MigrationRunner::new(crate::db::legacy_schema::legacy_schema_migrations())
            .run(&mut conn)
            .expect("legacy schema migrations apply to a fresh in-memory db");

        // Seed the exact 7 dead rows the recon found still being served by
        // `/graph/crates?workspace=nemo` from a long-gone root manifest,
        // plus a stale dependency edge among them.
        const STALE_CRATES: &[&str] = &[
            "nemo",
            "nemo-cookie",
            "nemo-db",
            "nemo-ml",
            "nemo-research",
            "nemo-viz",
            "trading-connectors-v4",
        ];
        for name in STALE_CRATES {
            writer::upsert_crate_node(
                &conn,
                &CrateNode {
                    workspace_id: "ws".to_string(),
                    name: name.to_string(),
                    version: "0.0.0".to_string(),
                    manifest_path: "Cargo.toml".to_string(),
                    source_dir: String::new(),
                    is_workspace_member: true,
                },
            )
            .expect("seed stale crate node");
        }
        writer::upsert_crate_dep_edge(
            &conn,
            &DepEdge {
                workspace_id: "ws".to_string(),
                from_crate: "nemo-db".to_string(),
                to_crate: "nemo".to_string(),
                is_path_dep: true,
                dep_path: Some("nemo".to_string()),
            },
        )
        .expect("seed stale dep edge");

        // The same prune-then-upsert sequence `Indexer::run`'s Phase 6
        // performs against the freshly parsed graph.
        let current_names: HashSet<String> = nodes.iter().map(|n| n.name.clone()).collect();
        let current_edges: HashSet<(String, String)> = edges
            .iter()
            .map(|e| (e.from_crate.clone(), e.to_crate.clone()))
            .collect();
        writer::delete_stale_crate_nodes(&conn, "ws", &current_names)
            .expect("prune stale crate nodes");
        writer::delete_stale_crate_dep_edges(&conn, "ws", &current_edges)
            .expect("prune stale crate dep edges");
        for node in &nodes {
            writer::upsert_crate_node(&conn, node).expect("upsert real crate node");
        }
        for edge in &edges {
            writer::upsert_crate_dep_edge(&conn, edge).expect("upsert real crate dep edge");
        }

        let stored_nodes = reader::list_crate_nodes(&conn, Some("ws")).expect("list crate nodes");
        let stored_names: HashSet<&str> = stored_nodes.iter().map(|n| n.name.as_str()).collect();
        for stale in STALE_CRATES {
            assert!(
                !stored_names.contains(stale),
                "stale crate node {stale} must be pruned, still present: {stored_names:?}"
            );
        }
        assert!(stored_names.contains("m4a-fixture-crate-a"));
        assert!(stored_names.contains("m4a-fixture-crate-b"));

        let stored_edges = reader::list_crate_deps(&conn, Some("ws")).expect("list crate dep edges");
        assert!(
            !stored_edges
                .iter()
                .any(|e| e.from_crate == "nemo-db" && e.to_crate == "nemo"),
            "stale dep edge nemo-db -> nemo must be pruned, still present: {stored_edges:?}"
        );
        assert!(
            stored_edges
                .iter()
                .any(|e| e.from_crate == "m4a-fixture-crate-b" && e.to_crate == "m4a-fixture-crate-a"),
            "real dep edge crate-b -> crate-a must survive the prune: {stored_edges:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
