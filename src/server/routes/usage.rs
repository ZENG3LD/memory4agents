use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::ast::known_types::extract_type_names;
use crate::db::reader;
use crate::server::state::{AppError, SharedState};
use crate::types::Symbol;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/ast/cluster", get(cluster))
        .route("/ast/parallel_systems", get(parallel_systems))
        .route("/ast/dead_in_group", get(dead_in_group))
        .route("/ast/type_usage", get(type_usage))
        .route("/ast/dead_types", get(dead_types))
        .route("/ast/usage_hotspots", get(usage_hotspots))
        .route("/ast/orphans", get(orphans))
}

// ---------------------------------------------------------------------------
// /ast/cluster
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ClusterQuery {
    seed: String,
    workspace: String,
    depth: Option<u32>,
    path_prefix: Option<String>,
    include_dead: Option<bool>,
}

#[derive(Serialize)]
struct ClusterNode {
    name: String,
    kind: String,
    file: String,
    line: u32,
    field_count: Option<u32>,
    callsite_count: u32,
    type_use_count: u32,
    shape_fingerprint: Option<String>,
    shape_summary: Option<String>,
}

#[derive(Serialize)]
struct ClusterEdge {
    from: String,
    to: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct ClusterStats {
    total_nodes: usize,
    total_edges: usize,
    dead_node_count: usize,
    depth_reached: u32,
}

#[derive(Serialize)]
struct ClusterResponse {
    seed: String,
    depth: u32,
    nodes: Vec<ClusterNode>,
    edges: Vec<ClusterEdge>,
    stats: ClusterStats,
}

async fn cluster(
    State(state): State<SharedState>,
    Query(params): Query<ClusterQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db.lock().map_err(|_| AppError::Internal("db lock poisoned".into()))?;
    let max_depth = params.depth.unwrap_or(2).min(4);
    let include_dead = params.include_dead.unwrap_or(true);
    let workspace = &params.workspace;

    // Load all structural symbols for fast lookup by name.
    let type_kinds: &[&str] = &["struct", "enum", "trait", "type_alias", "function", "async_function", "method", "class", "interface"];
    let all_syms = reader::get_all_symbols_filtered(&db, workspace, params.path_prefix.as_deref(), Some(type_kinds))?;

    // Build name → Vec<Symbol> index.
    let mut by_name: HashMap<String, Vec<Symbol>> = HashMap::new();
    for sym in all_syms {
        by_name.entry(sym.name.clone()).or_default().push(sym);
    }

    // Build callers index: name → Vec<name of callers>.
    // We use call_edges via reader::get_callers_of per visited node.
    // To avoid N DB queries, pre-build a reverse map from callee_name → caller symbol names.
    // For BFS at reasonable depth this is acceptable.

    let mut visited: HashMap<String, Symbol> = HashMap::new();
    let mut edges: Vec<ClusterEdge> = Vec::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    let mut depth_reached = 0u32;

    // Seed
    if let Some(seeds) = by_name.get(&params.seed) {
        for s in seeds {
            if !visited.contains_key(&s.name) {
                visited.insert(s.name.clone(), s.clone());
            }
            queue.push_back((s.name.clone(), 0));
        }
    }

    let mut edge_set: HashSet<(String, String, &'static str)> = HashSet::new();

    while let Some((current_name, depth)) = queue.pop_front() {
        if depth >= max_depth {
            depth_reached = depth_reached.max(depth);
            continue;
        }
        depth_reached = depth_reached.max(depth);

        // Get symbol signature to extract type dependencies.
        let sig = visited.get(&current_name).and_then(|s| s.signature.clone()).unwrap_or_default();
        let type_names = extract_type_names(&sig);

        for type_name in type_names {
            if let Some(syms) = by_name.get(&type_name) {
                for s in syms {
                    let key = (current_name.clone(), s.name.clone(), "uses");
                    if edge_set.insert(key) {
                        edges.push(ClusterEdge { from: current_name.clone(), to: s.name.clone(), kind: "uses" });
                    }
                    if !visited.contains_key(&s.name) {
                        visited.insert(s.name.clone(), s.clone());
                        queue.push_back((s.name.clone(), depth + 1));
                    }
                }
            }
        }

        // Get callers via DB.
        let callers = reader::get_callers_of(&db, &current_name, Some(workspace))?;
        for caller in callers {
            let key = (caller.name.clone(), current_name.clone(), "uses");
            if edge_set.insert(key) {
                edges.push(ClusterEdge { from: caller.name.clone(), to: current_name.clone(), kind: "uses" });
            }
            if !visited.contains_key(&caller.name) {
                let name = caller.name.clone();
                visited.insert(name.clone(), caller);
                queue.push_back((name, depth + 1));
            }
        }
    }

    // Build output nodes, applying include_dead filter.
    let mut nodes: Vec<ClusterNode> = visited
        .into_values()
        .filter(|s| include_dead || s.callsite_count > 0 || s.type_use_count > 0)
        .map(|s| ClusterNode {
            name: s.name,
            kind: s.kind.to_string(),
            file: s.file_path,
            line: s.line,
            field_count: s.field_count,
            callsite_count: s.callsite_count,
            type_use_count: s.type_use_count,
            shape_fingerprint: s.shape_fingerprint,
            shape_summary: s.shape_summary,
        })
        .collect();
    nodes.sort_by(|a, b| {
        let a_score = a.callsite_count + a.type_use_count;
        let b_score = b.callsite_count + b.type_use_count;
        b_score.cmp(&a_score).then(a.name.cmp(&b.name))
    });

    let dead_node_count = nodes.iter().filter(|n| n.callsite_count == 0 && n.type_use_count == 0).count();
    let total_nodes = nodes.len();
    let total_edges = edges.len();

    let resp = ClusterResponse {
        seed: params.seed,
        depth: max_depth,
        nodes,
        edges,
        stats: ClusterStats { total_nodes, total_edges, dead_node_count, depth_reached },
    };
    Ok(Json(serde_json::json!(resp)))
}

// ---------------------------------------------------------------------------
// /ast/parallel_systems
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ParallelSystemsQuery {
    workspace: String,
    path_prefix: Option<String>,
    min_size: Option<usize>,
}

#[derive(Serialize)]
struct ComponentInfo {
    id: usize,
    size: usize,
    kinds: HashMap<String, usize>,
    total_callsites: u64,
    dead_count: usize,
    names: Vec<String>,
    files: Vec<String>,
}

#[derive(Serialize)]
struct ParallelCandidate {
    component_a: usize,
    component_b: usize,
    profile_distance: f64,
    name_overlap: usize,
    rationale: String,
}

#[derive(Serialize)]
struct ParallelSystemsResponse {
    components: Vec<ComponentInfo>,
    parallel_candidates: Vec<ParallelCandidate>,
}

async fn parallel_systems(
    State(state): State<SharedState>,
    Query(params): Query<ParallelSystemsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db.lock().map_err(|_| AppError::Internal("db lock poisoned".into()))?;
    let min_size = params.min_size.unwrap_or(5);
    let workspace = &params.workspace;

    // Load structural symbols only.
    let struct_kinds: &[&str] = &["struct", "enum", "trait", "type_alias"];
    let all_syms = reader::get_all_symbols_filtered(
        &db,
        workspace,
        params.path_prefix.as_deref(),
        Some(struct_kinds),
    )?;

    // Build adjacency by signature type extraction.
    // name → index in all_syms (deduplicated by first occurrence).
    let mut name_to_idx: HashMap<String, usize> = HashMap::new();
    let mut sym_list: Vec<&Symbol> = Vec::new();
    for sym in &all_syms {
        name_to_idx.entry(sym.name.clone()).or_insert_with(|| {
            let idx = sym_list.len();
            sym_list.push(sym);
            idx
        });
    }

    let n = sym_list.len();
    // Adjacency as list of (a, b) undirected pairs.
    let mut adj: Vec<HashSet<usize>> = vec![HashSet::new(); n];

    for (idx, sym) in sym_list.iter().enumerate() {
        let sig = sym.signature.as_deref().unwrap_or("");
        let type_names = extract_type_names(sig);
        for type_name in type_names {
            if let Some(&other_idx) = name_to_idx.get(&type_name) {
                if other_idx != idx {
                    adj[idx].insert(other_idx);
                    adj[other_idx].insert(idx);
                }
            }
        }
    }

    // Union-Find for connected components.
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut Vec<usize>, x: usize) -> usize {
        if parent[x] != x {
            parent[x] = find(parent, parent[x]);
        }
        parent[x]
    }

    for i in 0..n {
        for &j in &adj[i] {
            let ri = find(&mut parent, i);
            let rj = find(&mut parent, j);
            if ri != rj {
                parent[ri] = rj;
            }
        }
    }

    // Group by component root.
    let mut comp_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        comp_map.entry(root).or_default().push(i);
    }

    // Build ComponentInfo for components >= min_size.
    let mut components: Vec<ComponentInfo> = Vec::new();
    for (comp_id, (_, members)) in comp_map.iter().enumerate() {
        if members.len() < min_size {
            continue;
        }
        let mut kinds: HashMap<String, usize> = HashMap::new();
        let mut total_callsites: u64 = 0;
        let mut dead_count = 0usize;
        let mut names: Vec<String> = Vec::new();
        let mut files_set: HashSet<String> = HashSet::new();

        for &idx in members {
            let sym = sym_list[idx];
            *kinds.entry(sym.kind.to_string()).or_insert(0) += 1;
            total_callsites += sym.callsite_count as u64;
            // is_dead: neither called nor used as type
            if sym.callsite_count == 0 && sym.type_use_count == 0 {
                dead_count += 1;
            }
            names.push(sym.name.clone());
            // Use directory prefix of file (up to second-to-last slash).
            let file_dir = sym.file_path.rsplitn(2, '/').last().unwrap_or(&sym.file_path).to_owned();
            files_set.insert(file_dir);
        }

        names.sort();
        let mut files: Vec<String> = files_set.into_iter().collect();
        files.sort();

        components.push(ComponentInfo {
            id: comp_id,
            size: members.len(),
            kinds,
            total_callsites,
            dead_count,
            names,
            files,
        });
    }

    // Re-assign sequential IDs and sort by size desc.
    components.sort_by(|a, b| b.size.cmp(&a.size));
    for (i, comp) in components.iter_mut().enumerate() {
        comp.id = i;
    }

    // Find parallel candidates: profile_distance < 0.2.
    let mut parallel_candidates: Vec<ParallelCandidate> = Vec::new();

    fn component_profile(comp: &ComponentInfo) -> [f64; 5] {
        let total = comp.size as f64;
        if total == 0.0 {
            return [0.0; 5];
        }
        let structs = *comp.kinds.get("struct").unwrap_or(&0) as f64 / total;
        let enums = *comp.kinds.get("enum").unwrap_or(&0) as f64 / total;
        let traits = *comp.kinds.get("trait").unwrap_or(&0) as f64 / total;
        let aliases = *comp.kinds.get("type_alias").unwrap_or(&0) as f64 / total;
        let dead = comp.dead_count as f64 / total;
        [structs, enums, traits, aliases, dead]
    }

    fn euclidean_dist(a: &[f64; 5], b: &[f64; 5]) -> f64 {
        a.iter().zip(b.iter()).map(|(x, y)| (x - y).powi(2)).sum::<f64>().sqrt()
    }

    for i in 0..components.len() {
        for j in (i + 1)..components.len() {
            let pa = component_profile(&components[i]);
            let pb = component_profile(&components[j]);
            let dist = euclidean_dist(&pa, &pb);
            if dist < 0.2 {
                // Count name overlap.
                let names_a: HashSet<&str> = components[i].names.iter().map(|s| s.as_str()).collect();
                let names_b: HashSet<&str> = components[j].names.iter().map(|s| s.as_str()).collect();
                let name_overlap = names_a.intersection(&names_b).count();
                let rationale = format!(
                    "similar struct/enum/trait counts (dist={:.3}); sizes {}/{}, callsites {}/{}",
                    dist,
                    components[i].size,
                    components[j].size,
                    components[i].total_callsites,
                    components[j].total_callsites,
                );
                parallel_candidates.push(ParallelCandidate {
                    component_a: components[i].id,
                    component_b: components[j].id,
                    profile_distance: (dist * 1000.0).round() / 1000.0,
                    name_overlap,
                    rationale,
                });
            }
        }
    }

    parallel_candidates.sort_by(|a, b| a.profile_distance.partial_cmp(&b.profile_distance).unwrap_or(std::cmp::Ordering::Equal));

    let resp = ParallelSystemsResponse { components, parallel_candidates };
    Ok(Json(serde_json::json!(resp)))
}

// ---------------------------------------------------------------------------
// /ast/dead_in_group
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeadInGroupQuery {
    fingerprint: String,
    workspace: String,
}

#[derive(Serialize)]
struct DeadInGroupEntry {
    name: String,
    kind: String,
    file: String,
    line: u32,
    callsite_count: u32,
    is_dead: bool,
}

#[derive(Serialize)]
struct DeadInGroupResponse {
    fingerprint: String,
    total: usize,
    dead_count: usize,
    members: Vec<DeadInGroupEntry>,
}

async fn dead_in_group(
    State(state): State<SharedState>,
    Query(params): Query<DeadInGroupQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db.lock().map_err(|_| AppError::Internal("db lock poisoned".into()))?;
    let symbols =
        reader::get_symbols_by_fingerprint(&db, &params.workspace, &params.fingerprint)?;

    let dead_count = symbols.iter().filter(|s| s.callsite_count == 0).count();
    let total = symbols.len();

    let members: Vec<DeadInGroupEntry> = symbols
        .into_iter()
        .map(|s| DeadInGroupEntry {
            is_dead: s.callsite_count == 0,
            name: s.name,
            kind: s.kind.to_string(),
            file: s.file_path,
            line: s.line,
            callsite_count: s.callsite_count,
        })
        .collect();

    let resp = DeadInGroupResponse {
        fingerprint: params.fingerprint,
        total,
        dead_count,
        members,
    };
    Ok(Json(serde_json::json!(resp)))
}

// ---------------------------------------------------------------------------
// /ast/type_usage
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TypeUsageQuery {
    name: String,
    workspace: String,
    path_prefix: Option<String>,
    breakdown: Option<bool>,
}

#[derive(Serialize)]
struct TypeUserEntry {
    name: String,
    kind: String,
    use_kind: String,
    file: String,
    line: u32,
}

#[derive(Serialize)]
struct TypeUsageMember {
    id: i64,
    file: String,
    line: u32,
    kind: String,
    type_use_count: u32,
    callsite_count: u32,
    users: Vec<TypeUserEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    users_by_kind: Option<HashMap<String, usize>>,
}

#[derive(Serialize)]
struct TypeUsageResponse {
    name: String,
    members: Vec<TypeUsageMember>,
}

async fn type_usage(
    State(state): State<SharedState>,
    Query(params): Query<TypeUsageQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db.lock().map_err(|_| AppError::Internal("db lock poisoned".into()))?;
    let symbols = reader::get_symbols_by_name_prefix(
        &db,
        &params.workspace,
        &params.name,
        params.path_prefix.as_deref(),
    )?;

    let want_breakdown = params.breakdown.unwrap_or(false);
    let mut members: Vec<TypeUsageMember> = Vec::new();
    for sym in symbols {
        let users_raw = reader::get_type_users(&db, &params.workspace, sym.id)?;
        let users_by_kind: Option<HashMap<String, usize>> = if want_breakdown {
            let mut map: HashMap<String, usize> = HashMap::new();
            for u in &users_raw {
                *map.entry(u.use_kind.clone()).or_insert(0) += 1;
            }
            Some(map)
        } else {
            None
        };
        let users: Vec<TypeUserEntry> = users_raw
            .into_iter()
            .map(|u| TypeUserEntry {
                name: u.name,
                kind: u.kind,
                use_kind: u.use_kind,
                file: u.file_path,
                line: u.line,
            })
            .collect();
        members.push(TypeUsageMember {
            id: sym.id,
            file: sym.file_path,
            line: sym.line,
            kind: sym.kind.to_string(),
            type_use_count: sym.type_use_count,
            callsite_count: sym.callsite_count,
            users,
            users_by_kind,
        });
    }

    let resp = TypeUsageResponse { name: params.name, members };
    Ok(Json(serde_json::json!(resp)))
}

// ---------------------------------------------------------------------------
// /ast/dead_types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeadTypesQuery {
    workspace: String,
    path_prefix: Option<String>,
    kind: Option<String>,
}

#[derive(Serialize)]
struct DeadTypeEntry {
    name: String,
    kind: String,
    file: String,
    line: u32,
    field_count: Option<u32>,
}

#[derive(Serialize)]
struct DeadTypesResponse {
    total: usize,
    dead: Vec<DeadTypeEntry>,
}

async fn dead_types(
    State(state): State<SharedState>,
    Query(params): Query<DeadTypesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db.lock().map_err(|_| AppError::Internal("db lock poisoned".into()))?;
    let symbols = reader::get_dead_types(
        &db,
        &params.workspace,
        params.path_prefix.as_deref(),
        params.kind.as_deref(),
    )?;

    let total = symbols.len();
    let dead: Vec<DeadTypeEntry> = symbols
        .into_iter()
        .map(|s| DeadTypeEntry {
            name: s.name,
            kind: s.kind.to_string(),
            file: s.file_path,
            line: s.line,
            field_count: s.field_count,
        })
        .collect();

    let resp = DeadTypesResponse { total, dead };
    Ok(Json(serde_json::json!(resp)))
}

// ---------------------------------------------------------------------------
// /ast/usage_hotspots
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UsageHotspotsQuery {
    workspace: String,
    path_prefix: Option<String>,
    top: Option<u32>,
}

#[derive(Serialize)]
struct UsageHotspotEntry {
    name: String,
    kind: String,
    file: String,
    line: u32,
    type_use_count: u32,
    callsite_count: u32,
}

#[derive(Serialize)]
struct UsageHotspotsResponse {
    hotspots: Vec<UsageHotspotEntry>,
}

async fn usage_hotspots(
    State(state): State<SharedState>,
    Query(params): Query<UsageHotspotsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db.lock().map_err(|_| AppError::Internal("db lock poisoned".into()))?;
    let top = params.top.unwrap_or(50);
    let symbols = reader::get_usage_hotspots(
        &db,
        &params.workspace,
        params.path_prefix.as_deref(),
        top,
    )?;

    let hotspots: Vec<UsageHotspotEntry> = symbols
        .into_iter()
        .map(|s| UsageHotspotEntry {
            name: s.name,
            kind: s.kind.to_string(),
            file: s.file_path,
            line: s.line,
            type_use_count: s.type_use_count,
            callsite_count: s.callsite_count,
        })
        .collect();

    let resp = UsageHotspotsResponse { hotspots };
    Ok(Json(serde_json::json!(resp)))
}

// ---------------------------------------------------------------------------
// /ast/orphans
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OrphansQuery {
    workspace: String,
    path_prefix: Option<String>,
    kind: Option<String>,
}

#[derive(Serialize)]
struct OrphanEntry {
    name: String,
    kind: String,
    file: String,
    line: u32,
    field_count: Option<u32>,
}

#[derive(Serialize)]
struct OrphansResponse {
    total: usize,
    orphans: Vec<OrphanEntry>,
}

async fn orphans(
    State(state): State<SharedState>,
    Query(params): Query<OrphansQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = state.db.lock().map_err(|_| AppError::Internal("db lock poisoned".into()))?;
    let symbols = reader::get_orphan_types(
        &db,
        &params.workspace,
        params.path_prefix.as_deref(),
        params.kind.as_deref(),
    )?;

    let total = symbols.len();
    let orphans_list: Vec<OrphanEntry> = symbols
        .into_iter()
        .map(|s| OrphanEntry {
            name: s.name,
            kind: s.kind.to_string(),
            file: s.file_path,
            line: s.line,
            field_count: s.field_count,
        })
        .collect();

    let resp = OrphansResponse { total, orphans: orphans_list };
    Ok(Json(serde_json::json!(resp)))
}

