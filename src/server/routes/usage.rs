use axum::{
    extract::{Query, State},
    Json,
};
use mcp_service4agent::router::{DocRouter, RouteDoc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::ast::known_types::{extract_type_names, tokenize_name};
use crate::ast::synonyms::{canonical_label, synonym_group};
use crate::db::reader;
use crate::server::state::{snapshot, with_db, AppError, SharedState};
use crate::types::Symbol;

pub fn router() -> DocRouter<SharedState> {
    DocRouter::new()
        .get(
            "/ast/cluster",
            cluster,
            RouteDoc::new(
                "BFS use-graph around a seed type: nodes + edges to depth N \
                 (?seed=NAME&workspace=ID&depth=N&path_prefix=P&include_dead=bool)",
            )
            .public(),
        )
        .get(
            "/ast/parallel_systems",
            parallel_systems,
            RouteDoc::new(
                "Weakly-connected components of the structural type graph plus parallel-candidate \
                 pairs by profile distance; mode=token for semantic name-token clustering \
                 (?workspace=ID&path_prefix=P&min_size=N&mode=wcc|token)",
            )
            .public(),
        )
        .get(
            "/ast/dead_in_group",
            dead_in_group,
            RouteDoc::new(
                "All symbols sharing a shape fingerprint, each flagged is_dead by callsite_count \
                 (?fingerprint=HASH&workspace=ID)",
            )
            .public(),
        )
        .get(
            "/ast/type_usage",
            type_usage,
            RouteDoc::new(
                "Who uses a name as a type: per-definition member list plus users from \
                 type_use_edges (?name=NAME&workspace=ID&path_prefix=P&breakdown=bool)",
            )
            .public(),
        )
        .get(
            "/ast/dead_types",
            dead_types,
            RouteDoc::new(
                "Symbols with zero type uses and zero callsites — dead candidates \
                 (?workspace=ID&path_prefix=P&kind=K)",
            )
            .public(),
        )
        .get(
            "/ast/usage_hotspots",
            usage_hotspots,
            RouteDoc::new("Top-N types by type_use_count desc (?workspace=ID&path_prefix=P&top=N)").public(),
        )
        .get(
            "/ast/orphans",
            orphans,
            RouteDoc::new(
                "Strict dead types: zero type uses, zero callsites, no meaningful type_use_edges \
                 row (?workspace=ID&path_prefix=P&kind=K&exclude_pub_use=bool)",
            )
            .public(),
        )
        .get(
            "/ast/role_groups",
            role_groups,
            RouteDoc::new(
                "Group types by shared name tokens, optionally merged under synonym canonicals \
                 (?workspace=ID&path_prefix=P&token=T&min_size=N&kind=K&use_synonyms=bool)",
            )
            .public(),
        )
        .get(
            "/ast/find_twins",
            find_twins,
            RouteDoc::new(
                "Find types similar to a seed by shape, shared/synonym tokens, or size; \
                 also implementors when the seed is a trait (?seed=NAME&workspace=ID&path_prefix=P)",
            )
            .public(),
        )
        .get(
            "/ast/workspace_summary",
            workspace_summary,
            RouteDoc::new(
                "High-level workspace report: totals, dead counts, hotspots, duplicate clusters, \
                 role groups, potential savings (?workspace=ID&path_prefix=P)",
            )
            .public(),
        )
        .get(
            "/ast/cleanup_plan",
            cleanup_plan,
            RouteDoc::new(
                "Prioritised cleanup action list — duplicate collapse, dead subsystem removal, \
                 synonym consolidation (?workspace=ID&path_prefix=P&limit=N)",
            )
            .public(),
        )
}

// ---------------------------------------------------------------------------
// /ast/cluster
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct ClusterQuery {
    pub(crate) seed: String,
    pub(crate) workspace: String,
    pub(crate) depth: Option<u32>,
    pub(crate) path_prefix: Option<String>,
    pub(crate) include_dead: Option<bool>,
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

pub(crate) async fn cluster_impl(state: &SharedState, params: ClusterQuery) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
    let max_depth = params.depth.unwrap_or(2).min(4);
    let include_dead = params.include_dead.unwrap_or(true);
    let workspace = &params.workspace;

    // Load all structural symbols for fast lookup by name.
    let type_kinds: &[&str] = &["struct", "enum", "trait", "type_alias", "function", "async_function", "method", "class", "interface"];
    let all_syms = reader::get_all_symbols_filtered(db, workspace, params.path_prefix.as_deref(), Some(type_kinds))?;

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
        let callers = reader::get_callers_of(db, &current_name, Some(workspace))?;
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
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn cluster(
    State(state): State<SharedState>,
    Query(params): Query<ClusterQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(cluster_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/parallel_systems
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct ParallelSystemsQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) min_size: Option<usize>,
    /// "wcc" (weakly-connected components, default) or "token" (semantic token clustering)
    pub(crate) mode: Option<String>,
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
struct TokenCandidatePair {
    group_a: String,
    group_b: String,
    similarity_score: f64,
    size_a: usize,
    size_b: usize,
    rationale: String,
}

#[derive(Serialize)]
struct TokenGroupInfo {
    token: String,
    size: usize,
    kinds: HashMap<String, usize>,
    field_count_avg: f64,
    total_type_use: u64,
    names: Vec<String>,
}

#[derive(Serialize)]
struct ParallelSystemsResponse {
    components: Vec<ComponentInfo>,
    parallel_candidates: Vec<ParallelCandidate>,
}

#[derive(Serialize)]
struct ParallelSystemsTokenResponse {
    groups: Vec<TokenGroupInfo>,
    candidate_pairs: Vec<TokenCandidatePair>,
}

pub(crate) async fn parallel_systems_impl(
    state: &SharedState,
    params: ParallelSystemsQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
    let min_size = params.min_size.unwrap_or(5);
    let workspace = &params.workspace;
    let mode = params.mode.as_deref().unwrap_or("wcc");

    if mode == "token" {
        return parallel_systems_token(db, workspace, params.path_prefix.as_deref(), min_size);
    }

    // Load structural symbols only.
    let struct_kinds: &[&str] = &["struct", "enum", "trait", "type_alias"];
    let all_syms = reader::get_all_symbols_filtered(
        db,
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

    for (i, neighbors) in adj.iter().enumerate() {
        for &j in neighbors {
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
    components.sort_by_key(|c| std::cmp::Reverse(c.size));
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
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn parallel_systems(
    State(state): State<SharedState>,
    Query(params): Query<ParallelSystemsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(parallel_systems_impl(&state, params).await?))
}

fn parallel_systems_token(
    db: &rusqlite::Connection,
    workspace: &str,
    path_prefix: Option<&str>,
    min_size: usize,
) -> Result<serde_json::Value, AppError> {
    let struct_kinds: &[&str] = &["struct", "enum", "trait", "type_alias"];
    let all_syms = reader::get_all_symbols_filtered(db, workspace, path_prefix, Some(struct_kinds))?;

    // Build token → symbol indices map
    let mut token_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, sym) in all_syms.iter().enumerate() {
        for token in tokenize_name(&sym.name) {
            token_to_indices.entry(token).or_default().push(idx);
        }
    }

    // Build groups, filter by min_size
    let mut groups: Vec<TokenGroupInfo> = Vec::new();
    for (token, indices) in &token_to_indices {
        if indices.len() < min_size {
            continue;
        }
        let mut kinds: HashMap<String, usize> = HashMap::new();
        let mut field_sum: u64 = 0;
        let mut field_count_n: u64 = 0;
        let mut total_type_use: u64 = 0;
        let mut names: Vec<String> = Vec::new();

        for &idx in indices {
            let sym = &all_syms[idx];
            *kinds.entry(sym.kind.to_string()).or_insert(0) += 1;
            if let Some(fc) = sym.field_count {
                field_sum += fc as u64;
                field_count_n += 1;
            }
            total_type_use += sym.type_use_count as u64;
            names.push(sym.name.clone());
        }
        names.sort();
        let field_count_avg = if field_count_n > 0 {
            (field_sum as f64 / field_count_n as f64 * 100.0).round() / 100.0
        } else {
            0.0
        };

        groups.push(TokenGroupInfo {
            token: token.clone(),
            size: indices.len(),
            kinds,
            field_count_avg,
            total_type_use,
            names,
        });
    }

    groups.sort_by(|a, b| b.size.cmp(&a.size).then(a.token.cmp(&b.token)));

    // Find candidate pairs: groups with similar profiles but no shared members
    // Profile: [struct%, enum%, trait%, alias%, field_count_avg_norm]
    fn group_profile(g: &TokenGroupInfo) -> [f64; 4] {
        let total = g.size as f64;
        if total == 0.0 {
            return [0.0; 4];
        }
        let structs = *g.kinds.get("struct").unwrap_or(&0) as f64 / total;
        let enums = *g.kinds.get("enum").unwrap_or(&0) as f64 / total;
        let traits = *g.kinds.get("trait").unwrap_or(&0) as f64 / total;
        let aliases = *g.kinds.get("type_alias").unwrap_or(&0) as f64 / total;
        [structs, enums, traits, aliases]
    }

    fn cosine_sim(a: &[f64; 4], b: &[f64; 4]) -> f64 {
        let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
        let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        (dot / (na * nb)).clamp(0.0, 1.0)
    }

    let mut candidate_pairs: Vec<TokenCandidatePair> = Vec::new();
    for i in 0..groups.len() {
        for j in (i + 1)..groups.len() {
            let a = &groups[i];
            let b = &groups[j];

            // No shared members check
            let names_a: HashSet<&str> = a.names.iter().map(|s| s.as_str()).collect();
            let names_b: HashSet<&str> = b.names.iter().map(|s| s.as_str()).collect();
            if !names_a.is_disjoint(&names_b) {
                continue; // groups overlap — not parallel
            }

            let pa = group_profile(a);
            let pb = group_profile(b);
            let score = cosine_sim(&pa, &pb);

            if score >= 0.7 && a.size >= min_size && b.size >= min_size {
                let rationale = format!(
                    "both groups dominated by {}/{}/{}/{}, sizes {}/{}, no shared members",
                    a.kinds.get("struct").copied().unwrap_or(0),
                    a.kinds.get("enum").copied().unwrap_or(0),
                    b.kinds.get("struct").copied().unwrap_or(0),
                    b.kinds.get("enum").copied().unwrap_or(0),
                    a.size,
                    b.size,
                );
                candidate_pairs.push(TokenCandidatePair {
                    group_a: a.token.clone(),
                    group_b: b.token.clone(),
                    similarity_score: (score * 1000.0).round() / 1000.0,
                    size_a: a.size,
                    size_b: b.size,
                    rationale,
                });
            }
        }
    }

    candidate_pairs.sort_by(|a, b| b.similarity_score.partial_cmp(&a.similarity_score).unwrap_or(std::cmp::Ordering::Equal));

    let resp = ParallelSystemsTokenResponse { groups, candidate_pairs };
    Ok(serde_json::json!(resp))
}

// ---------------------------------------------------------------------------
// /ast/dead_in_group
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct DeadInGroupQuery {
    pub(crate) fingerprint: String,
    pub(crate) workspace: String,
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

pub(crate) async fn dead_in_group_impl(
    state: &SharedState,
    params: DeadInGroupQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let symbols =
            reader::get_symbols_by_fingerprint(db, &params.workspace, &params.fingerprint)?;

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
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn dead_in_group(
    State(state): State<SharedState>,
    Query(params): Query<DeadInGroupQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(dead_in_group_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/type_usage
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct TypeUsageQuery {
    pub(crate) name: String,
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) breakdown: Option<bool>,
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

/// Piece 0.12: reads the read model — see
/// `Snapshot::symbols_by_name_prefix`/`Snapshot::type_users`.
pub(crate) async fn type_usage_impl(
    state: &SharedState,
    params: TypeUsageQuery,
) -> Result<serde_json::Value, AppError> {
    let snap = snapshot(state, &params.workspace)?;
    let symbols = snap.symbols_by_name_prefix(&params.name, params.path_prefix.as_deref());

    let want_breakdown = params.breakdown.unwrap_or(false);
    let mut members: Vec<TypeUsageMember> = Vec::new();
    for sym in symbols {
        let users_raw = snap.type_users(sym.id);
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
    Ok(serde_json::json!(resp))
}

async fn type_usage(
    State(state): State<SharedState>,
    Query(params): Query<TypeUsageQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(type_usage_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/dead_types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct DeadTypesQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) kind: Option<String>,
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

pub(crate) async fn dead_types_impl(
    state: &SharedState,
    params: DeadTypesQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let symbols = reader::get_dead_types(
            db,
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
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn dead_types(
    State(state): State<SharedState>,
    Query(params): Query<DeadTypesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(dead_types_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/usage_hotspots
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct UsageHotspotsQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) top: Option<u32>,
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

pub(crate) async fn usage_hotspots_impl(
    state: &SharedState,
    params: UsageHotspotsQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let top = params.top.unwrap_or(50);
        let symbols = reader::get_usage_hotspots(
            db,
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
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn usage_hotspots(
    State(state): State<SharedState>,
    Query(params): Query<UsageHotspotsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(usage_hotspots_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/orphans
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct OrphansQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) kind: Option<String>,
    /// When true (default), pub_use_decl edges don't count as real usage.
    pub(crate) exclude_pub_use: Option<bool>,
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

pub(crate) async fn orphans_impl(state: &SharedState, params: OrphansQuery) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let exclude_pub_use = params.exclude_pub_use.unwrap_or(true);
        let symbols = reader::get_orphan_types(
            db,
            &params.workspace,
            params.path_prefix.as_deref(),
            params.kind.as_deref(),
            exclude_pub_use,
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
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn orphans(
    State(state): State<SharedState>,
    Query(params): Query<OrphansQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(orphans_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/role_groups — group types by shared name tokens
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct RoleGroupsQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) min_size: Option<usize>,
    pub(crate) kind: Option<String>,
    /// When true, merge synonym tokens (style+theme+skin → canonical "style").
    pub(crate) use_synonyms: Option<bool>,
}

#[derive(Serialize)]
struct RoleGroupMember {
    name: String,
    kind: String,
    file: String,
    line: u32,
    field_count: Option<u32>,
    type_use_count: u32,
    tokens: Vec<String>,
}

#[derive(Serialize)]
struct RoleGroup {
    token: String,
    size: usize,
    members: Vec<RoleGroupMember>,
}

#[derive(Serialize)]
struct RoleGroupSynonym {
    canonical_token: String,
    raw_tokens: Vec<String>,
    size: usize,
    members: Vec<RoleGroupMember>,
}

#[derive(Serialize)]
struct RoleGroupsResponse {
    groups: Vec<RoleGroup>,
}

#[derive(Serialize)]
struct RoleGroupsSynonymResponse {
    groups: Vec<RoleGroupSynonym>,
}

pub(crate) async fn role_groups_impl(
    state: &SharedState,
    params: RoleGroupsQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
    let min_size = params.min_size.unwrap_or(3);
    let workspace = &params.workspace;
    let use_synonyms = params.use_synonyms.unwrap_or(false);

    let kind_filter: &[&str] = &["struct", "enum", "trait", "type_alias"];
    let all_syms = reader::get_all_symbols_filtered(
        db,
        workspace,
        params.path_prefix.as_deref(),
        Some(kind_filter),
    )?;

    // Optionally filter by kind
    let filtered: Vec<&crate::types::Symbol> = if let Some(ref k) = params.kind {
        all_syms.iter().filter(|s| s.kind.to_string() == *k).collect()
    } else {
        all_syms.iter().collect()
    };

    if use_synonyms {
        // canonical_token → (raw_tokens_seen, members)
        let mut canon_map: HashMap<String, (HashSet<String>, Vec<RoleGroupMember>)> = HashMap::new();

        for sym in &filtered {
            let tokens = tokenize_name(&sym.name);
            // Deduplicate canonicals per symbol so it doesn't appear multiple times in one group
            let mut seen_canonicals: HashSet<String> = HashSet::new();
            for token in &tokens {
                let canon = canonical_label(token);
                if let Some(ref requested) = params.token {
                    // token filter: match raw token or canonical
                    if token != requested && &canon != requested {
                        continue;
                    }
                }
                if seen_canonicals.insert(canon.clone()) {
                    let entry = canon_map.entry(canon.clone()).or_insert_with(|| (HashSet::new(), Vec::new()));
                    entry.0.insert(token.clone());
                    entry.1.push(RoleGroupMember {
                        name: sym.name.clone(),
                        kind: sym.kind.to_string(),
                        file: sym.file_path.clone(),
                        line: sym.line,
                        field_count: sym.field_count,
                        type_use_count: sym.type_use_count,
                        tokens: tokens.clone(),
                    });
                }
            }
        }

        let mut groups: Vec<RoleGroupSynonym> = canon_map
            .into_iter()
            .filter(|(_, (_, members))| members.len() >= min_size)
            .map(|(canonical_token, (raw_set, mut members))| {
                members.sort_by(|a, b| b.type_use_count.cmp(&a.type_use_count).then(a.name.cmp(&b.name)));
                let size = members.len();
                let mut raw_tokens: Vec<String> = raw_set.into_iter().collect();
                raw_tokens.sort();
                RoleGroupSynonym { canonical_token, raw_tokens, size, members }
            })
            .collect();

        groups.sort_by(|a, b| b.size.cmp(&a.size).then(a.canonical_token.cmp(&b.canonical_token)));
        return Ok(serde_json::json!(RoleGroupsSynonymResponse { groups }));
    }

    // Build token → members map (raw token mode)
    let mut token_map: HashMap<String, Vec<RoleGroupMember>> = HashMap::new();

    for sym in &filtered {
        let tokens = tokenize_name(&sym.name);
        for token in &tokens {
            // If specific token requested, only include matching
            if let Some(ref requested) = params.token {
                if token != requested {
                    continue;
                }
            }
            token_map.entry(token.clone()).or_default().push(RoleGroupMember {
                name: sym.name.clone(),
                kind: sym.kind.to_string(),
                file: sym.file_path.clone(),
                line: sym.line,
                field_count: sym.field_count,
                type_use_count: sym.type_use_count,
                tokens: tokens.clone(),
            });
        }
    }

    // Build groups, filter by min_size, sort members by type_use_count desc
    let mut groups: Vec<RoleGroup> = token_map
        .into_iter()
        .filter(|(_, members)| members.len() >= min_size)
        .map(|(token, mut members)| {
            members.sort_by(|a, b| b.type_use_count.cmp(&a.type_use_count).then(a.name.cmp(&b.name)));
            let size = members.len();
            RoleGroup { token, size, members }
        })
        .collect();

    // Sort groups by size desc
    groups.sort_by(|a, b| b.size.cmp(&a.size).then(a.token.cmp(&b.token)));

    let resp = RoleGroupsResponse { groups };
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn role_groups(
    State(state): State<SharedState>,
    Query(params): Query<RoleGroupsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(role_groups_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/find_twins — find types similar to a seed by multiple heuristics
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct FindTwinsQuery {
    pub(crate) seed: String,
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
}

#[derive(Serialize)]
struct TwinEntry {
    name: String,
    kind: String,
    file: String,
    line: u32,
    field_count: Option<u32>,
    type_use_count: u32,
}

#[derive(Serialize)]
struct SharedTokenTwin {
    name: String,
    kind: String,
    file: String,
    line: u32,
    field_count: Option<u32>,
    type_use_count: u32,
    shared_tokens: Vec<String>,
}

#[derive(Serialize)]
struct FindTwinsResponse {
    seed: String,
    twins: FindTwinsDetail,
}

#[derive(Serialize)]
struct SynonymTokenTwin {
    name: String,
    kind: String,
    file: String,
    line: u32,
    field_count: Option<u32>,
    type_use_count: u32,
    synonym_pair: [String; 2],
}

#[derive(Serialize)]
struct ImplementorEntry {
    #[serde(rename = "type")]
    type_name: String,
    file: String,
    line: u32,
    impl_id: i64,
}

#[derive(Serialize)]
struct FindTwinsDetail {
    exact_shape: Vec<TwinEntry>,
    loose_shape: Vec<TwinEntry>,
    shared_token: Vec<SharedTokenTwin>,
    synonym_token: Vec<SynonymTokenTwin>,
    similar_size: Vec<TwinEntry>,
    /// Populated when seed is a trait: all types that implement it.
    implementors: Vec<ImplementorEntry>,
}

pub(crate) async fn find_twins_impl(
    state: &SharedState,
    params: FindTwinsQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
    let workspace = &params.workspace;

    // Load the seed symbol(s)
    let kind_filter: &[&str] = &["struct", "enum", "trait", "type_alias"];
    let all_syms = reader::get_all_symbols_filtered(
        db,
        workspace,
        params.path_prefix.as_deref(),
        Some(kind_filter),
    )?;

    // Find seed
    let seed_syms: Vec<&crate::types::Symbol> = all_syms
        .iter()
        .filter(|s| s.name == params.seed)
        .collect();

    if seed_syms.is_empty() {
        return Ok(serde_json::json!({
            "error": format!("seed '{}' not found", params.seed)
        }));
    }

    let seed = seed_syms[0];
    let seed_raw_tokens: Vec<String> = tokenize_name(&seed.name);
    let seed_tokens: HashSet<String> = seed_raw_tokens.iter().cloned().collect();
    let seed_field_count = seed.field_count.unwrap_or(0);
    let seed_kind = seed.kind.to_string();

    // Pre-compute canonical tokens for seed (token → canonical)
    // Also build: canonical → first raw seed token that maps to it
    let mut seed_canon_to_raw: HashMap<String, String> = HashMap::new();
    for t in &seed_raw_tokens {
        let canon = canonical_label(t);
        seed_canon_to_raw.entry(canon).or_insert_with(|| t.clone());
    }

    let mut exact_shape: Vec<TwinEntry> = Vec::new();
    let mut loose_shape: Vec<TwinEntry> = Vec::new();
    let mut shared_token: Vec<SharedTokenTwin> = Vec::new();
    let mut synonym_token: Vec<SynonymTokenTwin> = Vec::new();
    let mut similar_size: Vec<TwinEntry> = Vec::new();

    for sym in &all_syms {
        if sym.name == params.seed {
            continue;
        }

        let sym_kind = sym.kind.to_string();
        let to_entry = || TwinEntry {
            name: sym.name.clone(),
            kind: sym_kind.clone(),
            file: sym.file_path.clone(),
            line: sym.line,
            field_count: sym.field_count,
            type_use_count: sym.type_use_count,
        };

        // a) Exact shape fingerprint
        if let (Some(a), Some(b)) = (&seed.shape_fingerprint, &sym.shape_fingerprint) {
            if a == b {
                exact_shape.push(to_entry());
                continue;
            }
        }

        // b) Loose shape fingerprint
        if let (Some(a), Some(b)) = (&seed.shape_fingerprint_loose, &sym.shape_fingerprint_loose) {
            if a == b && sym_kind == seed_kind {
                loose_shape.push(to_entry());
                continue;
            }
        }

        // c) Shared name tokens (exact token match)
        let sym_raw_tokens: Vec<String> = tokenize_name(&sym.name);
        let sym_tokens: HashSet<String> = sym_raw_tokens.iter().cloned().collect();
        let shared: Vec<String> = seed_tokens.intersection(&sym_tokens).cloned().collect();
        if !shared.is_empty() {
            let mut shared_sorted = shared;
            shared_sorted.sort();
            shared_token.push(SharedTokenTwin {
                name: sym.name.clone(),
                kind: sym_kind.clone(),
                file: sym.file_path.clone(),
                line: sym.line,
                field_count: sym.field_count,
                type_use_count: sym.type_use_count,
                shared_tokens: shared_sorted,
            });
            continue;
        }

        // d) Synonym token — a token in sym maps to a canonical that seed also has
        let mut synonym_pair_found: Option<[String; 2]> = None;
        'outer: for sym_tok in &sym_raw_tokens {
            let sym_canon = canonical_label(sym_tok);
            // Only consider tokens that ARE in a synonym group (non-trivial)
            if synonym_group(sym_tok).is_empty() {
                continue;
            }
            if let Some(seed_raw) = seed_canon_to_raw.get(&sym_canon) {
                // Make sure they're not the same raw token (that's shared_token above)
                if seed_raw != sym_tok {
                    synonym_pair_found = Some([seed_raw.clone(), sym_tok.clone()]);
                    break 'outer;
                }
            }
        }
        if let Some(pair) = synonym_pair_found {
            synonym_token.push(SynonymTokenTwin {
                name: sym.name.clone(),
                kind: sym_kind.clone(),
                file: sym.file_path.clone(),
                line: sym.line,
                field_count: sym.field_count,
                type_use_count: sym.type_use_count,
                synonym_pair: pair,
            });
            continue;
        }

        // e) Similar field/method count ±1, same kind
        if sym_kind == seed_kind {
            let sym_fc = sym.field_count.unwrap_or(0);
            if sym_fc.abs_diff(seed_field_count) <= 1 {
                similar_size.push(to_entry());
            }
        }
    }

    // Sort all by type_use_count desc
    exact_shape.sort_by_key(|t| std::cmp::Reverse(t.type_use_count));
    loose_shape.sort_by_key(|t| std::cmp::Reverse(t.type_use_count));
    shared_token.sort_by(|a, b| b.type_use_count.cmp(&a.type_use_count).then(a.name.cmp(&b.name)));
    synonym_token.sort_by(|a, b| b.type_use_count.cmp(&a.type_use_count).then(a.name.cmp(&b.name)));
    similar_size.sort_by(|a, b| b.type_use_count.cmp(&a.type_use_count).then(a.name.cmp(&b.name)));

    // If seed is a trait, fetch all implementors.
    let implementors: Vec<ImplementorEntry> = if seed.kind == crate::types::SymbolKind::Trait {
        reader::get_impls_of_trait(
            db,
            workspace,
            &params.seed,
            params.path_prefix.as_deref(),
        )
        .unwrap_or_default()
        .into_iter()
        .map(|s| ImplementorEntry {
            type_name: s.name,
            file: s.file_path,
            line: s.line,
            impl_id: s.id,
        })
        .collect()
    } else {
        Vec::new()
    };

    let resp = FindTwinsResponse {
        seed: params.seed,
        twins: FindTwinsDetail {
            exact_shape,
            loose_shape,
            shared_token,
            synonym_token,
            similar_size,
            implementors,
        },
    };
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn find_twins(
    State(state): State<SharedState>,
    Query(params): Query<FindTwinsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(find_twins_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/workspace_summary — high-level metrics for a workspace
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct WorkspaceSummaryQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
}

#[derive(Serialize)]
struct WsTotals {
    files: usize,
    symbols: usize,
    structs: usize,
    enums: usize,
    traits: usize,
    type_aliases: usize,
    modules: usize,
    functions: usize,
}

#[derive(Serialize)]
struct WsDeadSubsystem {
    path: String,
    dead_count: usize,
}

#[derive(Serialize)]
struct WsDead {
    structs: usize,
    enums: usize,
    traits: usize,
    by_subsystem: Vec<WsDeadSubsystem>,
}

#[derive(Serialize)]
struct WsHotspot {
    name: String,
    type_use_count: u32,
}

#[derive(Serialize)]
struct WsBiggestDupGroup {
    shape_summary: String,
    names_count: usize,
    members: Vec<String>,
}

#[derive(Serialize)]
struct WsDuplicates {
    exact_count: usize,
    loose_count: usize,
    biggest_strict: Vec<WsBiggestDupGroup>,
}

#[derive(Serialize)]
struct WsRoleGroup {
    canonical_token: String,
    size: usize,
    raw_tokens: Vec<String>,
}

#[derive(Serialize)]
struct WsPotentialSavings {
    exact_dup_locations: usize,
    loose_dup_locations: usize,
    estimated_lines_drop: usize,
}

#[derive(Serialize)]
struct WorkspaceSummaryResponse {
    totals: WsTotals,
    dead: WsDead,
    hotspots_top10: Vec<WsHotspot>,
    duplicate_clusters: WsDuplicates,
    role_groups_top: Vec<WsRoleGroup>,
    potential_savings: WsPotentialSavings,
}

pub(crate) async fn workspace_summary_impl(
    state: &SharedState,
    params: WorkspaceSummaryQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
    let workspace = &params.workspace;
    let path_prefix = params.path_prefix.as_deref();

    // All symbols (all kinds for counts)
    let all_kinds: &[&str] = &[
        "struct", "enum", "trait", "type_alias", "module",
        "function", "async_function", "method",
    ];
    let all_syms =
        reader::get_all_symbols_filtered(db, workspace, path_prefix, Some(all_kinds))?;

    // Count by kind
    let mut n_structs = 0usize;
    let mut n_enums = 0usize;
    let mut n_traits = 0usize;
    let mut n_aliases = 0usize;
    let mut n_modules = 0usize;
    let mut n_functions = 0usize;

    for s in &all_syms {
        match s.kind.to_string().as_str() {
            "struct" => n_structs += 1,
            "enum" => n_enums += 1,
            "trait" => n_traits += 1,
            "type_alias" => n_aliases += 1,
            "module" => n_modules += 1,
            "function" | "async_function" | "method" => n_functions += 1,
            _ => {}
        }
    }

    // Unique files
    let file_set: HashSet<&str> = all_syms.iter().map(|s| s.file_path.as_str()).collect();
    let n_files = file_set.len();

    // Dead structural types
    let struct_kind_strs: &[&str] = &["struct", "enum", "trait", "type_alias"];
    let struct_kinds_owned: Vec<String> =
        struct_kind_strs.iter().map(|s| (*s).to_owned()).collect();
    let dead_syms =
        reader::get_dead_types(db, workspace, path_prefix, None)?;

    let dead_structs = dead_syms.iter().filter(|s| s.kind.to_string() == "struct").count();
    let dead_enums = dead_syms.iter().filter(|s| s.kind.to_string() == "enum").count();
    let dead_traits = dead_syms.iter().filter(|s| s.kind.to_string() == "trait").count();

    // Dead by subsystem (2-level path prefix grouping)
    let mut subsys_dead: HashMap<String, usize> = HashMap::new();
    for s in &dead_syms {
        let prefix = subsystem_prefix(&s.file_path, 2);
        *subsys_dead.entry(prefix).or_insert(0) += 1;
    }
    let mut by_subsystem: Vec<WsDeadSubsystem> = subsys_dead
        .into_iter()
        .map(|(path, dead_count)| WsDeadSubsystem { path, dead_count })
        .collect();
    by_subsystem.sort_by_key(|s| std::cmp::Reverse(s.dead_count));
    by_subsystem.truncate(10);

    // Hotspots top 10
    let hot_syms = reader::get_usage_hotspots(db, workspace, path_prefix, 10)?;
    let hotspots_top10: Vec<WsHotspot> = hot_syms
        .into_iter()
        .map(|s| WsHotspot { name: s.name, type_use_count: s.type_use_count })
        .collect();

    // Semantic duplicate clusters (strict + loose)
    let strict_rows = reader::get_semantic_dup_rows(
        db,
        reader::SemanticDupOptions {
            workspace_id: workspace,
            path_prefix,
            kinds: Some(&struct_kinds_owned),
            loose: false,
        },
    )?;
    let loose_rows = reader::get_semantic_dup_rows(
        db,
        reader::SemanticDupOptions {
            workspace_id: workspace,
            path_prefix,
            kinds: Some(&struct_kinds_owned),
            loose: true,
        },
    )?;

    let (exact_count, exact_locations, biggest_strict) = summarise_dup_rows(strict_rows, 5);
    let (loose_count, loose_locations, _) = summarise_dup_rows(loose_rows, 0);

    // Estimated savings: (locations - group_count) * avg_lines_per_member
    let avg_lines: usize = 15;
    let estimated_lines_drop = exact_locations.saturating_sub(exact_count) * avg_lines;

    // Role groups top (with synonyms)
    let struct_syms =
        reader::get_all_symbols_filtered(db, workspace, path_prefix, Some(struct_kind_strs))?;
    let mut canon_map: HashMap<String, (HashSet<String>, usize)> = HashMap::new();
    for sym in &struct_syms {
        let tokens = tokenize_name(&sym.name);
        let mut seen: HashSet<String> = HashSet::new();
        for t in &tokens {
            let canon = canonical_label(t);
            if seen.insert(canon.clone()) {
                let e = canon_map.entry(canon).or_insert_with(|| (HashSet::new(), 0));
                e.0.insert(t.clone());
                e.1 += 1;
            }
        }
    }
    let mut role_groups_top: Vec<WsRoleGroup> = canon_map
        .into_iter()
        .filter(|(_, (_, sz))| *sz >= 3)
        .map(|(canonical_token, (raw_set, size))| {
            let mut raw_tokens: Vec<String> = raw_set.into_iter().collect();
            raw_tokens.sort();
            WsRoleGroup { canonical_token, size, raw_tokens }
        })
        .collect();
    role_groups_top.sort_by(|a, b| b.size.cmp(&a.size).then(a.canonical_token.cmp(&b.canonical_token)));
    role_groups_top.truncate(10);

    let resp = WorkspaceSummaryResponse {
        totals: WsTotals {
            files: n_files,
            symbols: all_syms.len(),
            structs: n_structs,
            enums: n_enums,
            traits: n_traits,
            type_aliases: n_aliases,
            modules: n_modules,
            functions: n_functions,
        },
        dead: WsDead {
            structs: dead_structs,
            enums: dead_enums,
            traits: dead_traits,
            by_subsystem,
        },
        hotspots_top10,
        duplicate_clusters: WsDuplicates {
            exact_count,
            loose_count,
            biggest_strict,
        },
        role_groups_top,
        potential_savings: WsPotentialSavings {
            exact_dup_locations: exact_locations,
            loose_dup_locations: loose_locations,
            estimated_lines_drop,
        },
    };
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn workspace_summary(
    State(state): State<SharedState>,
    Query(params): Query<WorkspaceSummaryQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(workspace_summary_impl(&state, params).await?))
}

/// Extract a 1- or 2-level path prefix from a file_path string.
/// E.g. "ui/animation/recipes/foo.rs" → "ui/animation/" (depth=2).
fn subsystem_prefix(file_path: &str, depth: usize) -> String {
    // Normalise separators
    let normed: String = file_path.replace('\\', "/");
    let parts: Vec<&str> = normed.splitn(depth + 1, '/').collect();
    if parts.len() <= depth {
        normed
    } else {
        parts[..depth].join("/") + "/"
    }
}

/// Group semantic dup rows by fingerprint; return (group_count, total_locations, top_N biggest).
fn summarise_dup_rows(
    rows: Vec<reader::SemanticDupRow>,
    top_n: usize,
) -> (usize, usize, Vec<WsBiggestDupGroup>) {
    let mut fp_map: HashMap<String, (Option<String>, Vec<String>)> = HashMap::new();
    for row in rows {
        let e = fp_map
            .entry(row.shape_fingerprint.clone())
            .or_insert_with(|| (row.shape_summary.clone(), Vec::new()));
        if e.0.is_none() {
            e.0 = row.shape_summary;
        }
        e.1.push(row.name);
    }

    // Filter: need >=2 distinct names
    let mut groups: Vec<(String, Option<String>, Vec<String>)> = fp_map
        .into_iter()
        .filter_map(|(fp, (summary, names))| {
            let unique: HashSet<String> = names.iter().cloned().collect();
            if unique.len() >= 2 {
                let mut sorted: Vec<String> = unique.into_iter().collect();
                sorted.sort();
                Some((fp, summary, sorted))
            } else {
                None
            }
        })
        .collect();

    let total_locations: usize = groups.iter().map(|(_, _, names)| names.len()).sum();
    let group_count = groups.len();

    groups.sort_by_key(|g| std::cmp::Reverse(g.2.len()));

    let biggest: Vec<WsBiggestDupGroup> = groups
        .into_iter()
        .take(top_n)
        .map(|(_fp, summary, names)| {
            let names_count = names.len();
            WsBiggestDupGroup {
                shape_summary: summary.unwrap_or_default(),
                names_count,
                members: names,
            }
        })
        .collect();

    (group_count, total_locations, biggest)
}

// ---------------------------------------------------------------------------
// /ast/cleanup_plan — prioritised list of cleanup actions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct CleanupPlanQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) limit: Option<usize>,
}

#[derive(Serialize)]
struct CleanupAction {
    priority: usize,
    kind: &'static str,
    description: String,
    members: Vec<String>,
    estimated_savings_lines: usize,
}

#[derive(Serialize)]
struct CleanupPlanResponse {
    plan: Vec<CleanupAction>,
}

pub(crate) async fn cleanup_plan_impl(
    state: &SharedState,
    params: CleanupPlanQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
    let workspace = &params.workspace;
    let path_prefix = params.path_prefix.as_deref();
    let limit = params.limit.unwrap_or(20);

    let struct_kind_strs: &[&str] = &["struct", "enum", "trait", "type_alias"];
    let struct_kinds_owned: Vec<String> =
        struct_kind_strs.iter().map(|s| (*s).to_owned()).collect();

    // --- 1. Exact duplicate groups ---
    let strict_rows = reader::get_semantic_dup_rows(
        db,
        reader::SemanticDupOptions {
            workspace_id: workspace,
            path_prefix,
            kinds: Some(&struct_kinds_owned),
            loose: false,
        },
    )?;

    let mut fp_strict: HashMap<String, (Option<String>, Vec<String>)> = HashMap::new();
    for row in strict_rows {
        let e = fp_strict
            .entry(row.shape_fingerprint.clone())
            .or_insert_with(|| (row.shape_summary.clone(), Vec::new()));
        if e.0.is_none() {
            e.0 = row.shape_summary;
        }
        e.1.push(row.name);
    }

    let mut actions: Vec<CleanupAction> = Vec::new();

    for (_, (summary, names)) in fp_strict {
        let unique: HashSet<String> = names.iter().cloned().collect();
        if unique.len() < 2 {
            continue;
        }
        let mut sorted: Vec<String> = unique.into_iter().collect();
        sorted.sort();
        let savings = sorted.len().saturating_sub(1) * 15;
        let desc = format!(
            "{} newtype({}) — collapse to one canonical",
            sorted.len(),
            summary.as_deref().unwrap_or("?"),
        );
        actions.push(CleanupAction {
            priority: 0, // filled in later
            kind: "exact_duplicate_collapse",
            description: desc,
            members: sorted,
            estimated_savings_lines: savings,
        });
    }

    // Sort exact dups by savings desc
    actions.sort_by_key(|a| std::cmp::Reverse(a.estimated_savings_lines));

    // --- 2. Dead subsystems ---
    let dead_syms = reader::get_dead_types(db, workspace, path_prefix, None)?;
    let mut subsys_dead: HashMap<String, Vec<String>> = HashMap::new();
    for s in &dead_syms {
        let prefix = subsystem_prefix(&s.file_path, 3);
        subsys_dead.entry(prefix).or_default().push(s.name.clone());
    }
    let mut dead_subsys_actions: Vec<CleanupAction> = subsys_dead
        .into_iter()
        .filter(|(_, names)| names.len() >= 5)
        .map(|(path, mut names)| {
            names.sort();
            let cnt = names.len();
            let savings = cnt * 15;
            let desc = format!("{path} — {cnt} unused types");
            CleanupAction {
                priority: 0,
                kind: "dead_subsystem_remove",
                description: desc,
                members: names,
                estimated_savings_lines: savings,
            }
        })
        .collect();
    dead_subsys_actions.sort_by_key(|a| std::cmp::Reverse(a.estimated_savings_lines));

    // --- 3. Synonym role consolidation ---
    let struct_syms =
        reader::get_all_symbols_filtered(db, workspace, path_prefix, Some(struct_kind_strs))?;
    let mut canon_map: HashMap<String, (HashSet<String>, usize)> = HashMap::new();
    for sym in &struct_syms {
        let tokens = tokenize_name(&sym.name);
        let mut seen: HashSet<String> = HashSet::new();
        for t in &tokens {
            let canon = canonical_label(t);
            if seen.insert(canon.clone()) {
                let e = canon_map.entry(canon).or_insert_with(|| (HashSet::new(), 0));
                e.0.insert(t.clone());
                e.1 += 1;
            }
        }
    }
    let mut synonym_actions: Vec<CleanupAction> = canon_map
        .into_iter()
        .filter(|(_, (raw_set, sz))| *sz >= 5 && raw_set.len() >= 2)
        .map(|(canonical, (raw_set, sz))| {
            let mut raw_tokens: Vec<String> = raw_set.into_iter().collect();
            raw_tokens.sort();
            let desc = format!(
                "{} — unify under one token (raw: {})",
                canonical,
                raw_tokens.join(", ")
            );
            let savings = sz.saturating_sub(1) * 5; // rough: consolidate naming effort
            CleanupAction {
                priority: 0,
                kind: "synonym_role_consolidate",
                description: desc,
                members: raw_tokens,
                estimated_savings_lines: savings,
            }
        })
        .collect();
    synonym_actions.sort_by_key(|a| std::cmp::Reverse(a.estimated_savings_lines));

    // Merge all in priority order
    let mut all_actions: Vec<CleanupAction> = Vec::new();
    all_actions.extend(actions);
    all_actions.extend(dead_subsys_actions);
    all_actions.extend(synonym_actions);
    all_actions.truncate(limit);

    // Assign sequential priority numbers
    for (i, action) in all_actions.iter_mut().enumerate() {
        action.priority = i + 1;
    }

    let resp = CleanupPlanResponse { plan: all_actions };
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn cleanup_plan(
    State(state): State<SharedState>,
    Query(params): Query<CleanupPlanQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(cleanup_plan_impl(&state, params).await?))
}
