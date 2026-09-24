use axum::{
    extract::{Query, State},
    Json,
};
use mcp_service4agent::router::{DocRouter, RouteDoc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::HashMap;
use crate::db::reader;
use crate::server::state::{snapshot, snapshots_for, with_db, AppError, SharedState};
use crate::types::{SearchHit, Symbol};

/// A bare name defined more than this many times in a workspace is refused
/// by [`callers_of_symbol_impl`] with a named "ambiguous" response instead
/// of computing callers for every one of them — the same shared `*_impl`
/// function both `/ast/callers` (HTTP) and `mem_symbol what="callers"`
/// (MCP) call, so one guard covers both doors. Reproduces the reported
/// hang: `"new"` has 18,416 definitions and ~139k distinct call sites in
/// the live `nemo` workspace (2026-09-24 read-path audit) — computing
/// "who calls new" is not a meaningful question without narrowing by file
/// or type first.
pub(crate) const AMBIGUOUS_DEFINITION_THRESHOLD: u32 = 20;

pub fn router() -> DocRouter<SharedState> {
    DocRouter::new()
        .get("/ast/search", search_symbols, RouteDoc::new("Fuzzy symbol search by name (?q=NAME)").public())
        .get(
            "/ast/symbols",
            symbols_in_file,
            RouteDoc::new("List all symbols in a file (?file=PATH&workspace=WS)").public(),
        )
        .get(
            "/ast/callers",
            callers_of_symbol,
            RouteDoc::new("Find callers of a symbol (?sym=NAME&workspace=WS)").public(),
        )
        .get("/ast/deps", file_deps, RouteDoc::new("File dependency list (?file=PATH&workspace=WS)").public())
        .get(
            "/ast/duplicates",
            duplicates_of_name,
            RouteDoc::new(
                "List all definitions of a name, classified exact/structural/name-only \
                 (?name=NAME&workspace=ID&kind=KIND)",
            )
            .public(),
        )
        .get(
            "/ast/all_duplicates",
            all_duplicates,
            RouteDoc::new("All symbols with 2+ definitions, ranked by count (?workspace=ID&min_count=N&kind=KIND)")
                .public(),
        )
        .get(
            "/ast/semantic_duplicates",
            semantic_duplicates,
            RouteDoc::new(
                "Type-shape duplicates: same struct fields/enum variants/trait methods under \
                 different names (?workspace=ID&path_prefix=P&kind=K&loose=bool&min_names=N&\
                 min_locations=N&min_fields=N&max_fields=N)",
            )
            .public(),
        )
        .get(
            "/ast/impls",
            impls_of_trait,
            RouteDoc::new("All types implementing a trait (?trait=NAME&workspace=ID&path_prefix=P)").public(),
        )
        .get(
            "/ast/inherent_impls",
            inherent_impls_of_type,
            RouteDoc::new("Inherent impl blocks for a type, no trait (?type=NAME&workspace=ID&path_prefix=P)")
                .public(),
        )
        .get(
            "/ast/impl_summary",
            impl_summary,
            RouteDoc::new("All traits with their implementors, sorted by implementor count desc (?workspace=ID&path_prefix=P)")
                .public(),
        )
}

#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    pub(crate) q: String,
    pub(crate) workspace: Option<String>,
    pub(crate) limit: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    pub(crate) file: String,
    pub(crate) workspace: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct SymQuery {
    pub(crate) sym: String,
    pub(crate) workspace: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct DuplicatesQuery {
    pub(crate) name: String,
    pub(crate) workspace: Option<String>,
    pub(crate) kind: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct AllDuplicatesQuery {
    pub(crate) workspace: String,
    pub(crate) min_count: Option<u32>,
    pub(crate) kind: Option<String>,
}

/// Piece 0.12: reads the in-memory read model instead of the trigram table
/// in SQL — see `crate::model::snapshot::Snapshot::fuzzy_search`. Merges
/// across every loaded workspace when `params.workspace` is `None`,
/// mirroring `search::trigram::fuzzy_search`'s own `workspace_id: None`
/// convention.
pub(crate) async fn search_symbols_impl(
    state: &SharedState,
    params: SearchQuery,
) -> Result<serde_json::Value, AppError> {
    let limit = params.limit.unwrap_or(50);
    let snaps = snapshots_for(state, params.workspace.as_deref())?;
    let mut hits: Vec<SearchHit> = Vec::new();
    for snap in &snaps {
        hits.extend(snap.fuzzy_search(&params.q, limit, 0.1));
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    hits.truncate(limit as usize);
    Ok(serde_json::json!(hits))
}

async fn search_symbols(
    State(state): State<SharedState>,
    Query(params): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(search_symbols_impl(&state, params).await?))
}

/// Piece 0.12: reads the read model — see `Snapshot::symbols_in_file`.
pub(crate) async fn symbols_in_file_impl(
    state: &SharedState,
    params: FileQuery,
) -> Result<serde_json::Value, AppError> {
    let workspace = params.workspace.as_deref().unwrap_or("");
    let snap = snapshot(state, workspace)?;
    Ok(serde_json::json!(snap.symbols_in_file(&params.file)))
}

async fn symbols_in_file(
    State(state): State<SharedState>,
    Query(params): Query<FileQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(symbols_in_file_impl(&state, params).await?))
}

/// Piece 0.12: reads the read model — see `Snapshot::count_definitions`/
/// `Snapshot::callers_of`. Merges across every loaded workspace when
/// `params.workspace` is `None`, same convention as `search_symbols_impl`.
pub(crate) async fn callers_of_symbol_impl(
    state: &SharedState,
    params: SymQuery,
) -> Result<serde_json::Value, AppError> {
    let snaps = snapshots_for(state, params.workspace.as_deref())?;
    let definition_count: u32 = snaps.iter().map(|s| s.count_definitions(&params.sym)).sum();
    if definition_count > AMBIGUOUS_DEFINITION_THRESHOLD {
        return Ok(serde_json::json!({
            "ambiguous": true,
            "sym": params.sym,
            "definition_count": definition_count,
            "message": format!(
                "ambiguous: {definition_count} definitions of '{}' — pass a file or type to disambiguate",
                params.sym
            ),
        }));
    }
    let mut results: Vec<Symbol> = Vec::new();
    for snap in &snaps {
        if results.len() >= crate::model::MAX_CALLERS {
            break;
        }
        results.extend(snap.callers_of(&params.sym));
    }
    results.truncate(crate::model::MAX_CALLERS);
    Ok(serde_json::json!(results))
}

async fn callers_of_symbol(
    State(state): State<SharedState>,
    Query(params): Query<SymQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(callers_of_symbol_impl(&state, params).await?))
}

/// Piece 0.12: reads the read model — see `Snapshot::deps_of_file`.
pub(crate) async fn file_deps_impl(state: &SharedState, params: FileQuery) -> Result<serde_json::Value, AppError> {
    let workspace = params.workspace.as_deref().unwrap_or("");
    let snap = snapshot(state, workspace)?;
    Ok(serde_json::json!(snap.deps_of_file(&params.file)))
}

async fn file_deps(
    State(state): State<SharedState>,
    Query(params): Query<FileQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(file_deps_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// Helpers for duplicate classification
// ---------------------------------------------------------------------------

fn normalize_for_hash(text: &str) -> String {
    // Strip block comments /* ... */
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    // Collapse whitespace
    let mut result = String::with_capacity(out.len());
    let mut prev_space = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(ch);
            prev_space = false;
        }
    }
    result.trim().to_owned()
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sig_hash(sig: &Option<String>) -> String {
    match sig {
        Some(s) => sha256_hex(&normalize_for_hash(s)),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DuplicateMember {
    file: String,
    line: u32,
    kind: String,
    visibility: String,
}

#[derive(Serialize)]
struct DuplicateGroup {
    classification: &'static str,
    signature_hash: String,
    body_hash: Option<String>,
    shape_fingerprint: Option<String>,
    members: Vec<DuplicateMember>,
}

#[derive(Serialize)]
struct DuplicatesResponse {
    name: String,
    workspace: Option<String>,
    groups: Vec<DuplicateGroup>,
    total_locations: usize,
}

#[derive(Serialize)]
struct AllDuplicateEntry {
    name: String,
    kind: String,
    total: u32,
    exact_groups: usize,
    structural_groups: usize,
    name_only_count: usize,
}

#[derive(Serialize)]
struct AllDuplicatesResponse {
    workspace: String,
    duplicates: Vec<AllDuplicateEntry>,
}

// ---------------------------------------------------------------------------
// classify: group symbols by signature/body
// ---------------------------------------------------------------------------

fn classify_symbols(symbols: Vec<Symbol>) -> Vec<DuplicateGroup> {
    // Group by (sig_hash, body_hash)
    // Key: (sig_hash, body_hash_or_empty)
    // Also track shape_fingerprint per sig_hash (take first non-None)
    let mut exact_map: HashMap<(String, String), Vec<DuplicateMember>> = HashMap::new();
    let mut fp_map: HashMap<String, Option<String>> = HashMap::new(); // sig_hash → shape_fingerprint

    for sym in &symbols {
        let sh = sig_hash(&sym.signature);
        let bh = sym.body_hash.clone().unwrap_or_default();
        let member = DuplicateMember {
            file: sym.file_path.clone(),
            line: sym.line,
            kind: sym.kind.to_string(),
            visibility: sym.visibility.to_string(),
        };
        exact_map.entry((sh.clone(), bh)).or_default().push(member);
        fp_map.entry(sh).or_insert_with(|| sym.shape_fingerprint.clone());
    }

    // Now promote: same sig, different body → structural; sig empty → name-only
    // Collect sig_hash → list of (body_hash, members)
    let mut sig_map: HashMap<String, Vec<(String, Vec<DuplicateMember>)>> = HashMap::new();
    for ((sh, bh), members) in exact_map {
        sig_map.entry(sh).or_default().push((bh, members));
    }

    let mut groups: Vec<DuplicateGroup> = Vec::new();

    for (sh, body_groups) in sig_map {
        let fp = fp_map.get(&sh).and_then(|v| v.clone());
        if sh.is_empty() {
            // No signature at all — name-only
            let members: Vec<DuplicateMember> =
                body_groups.into_iter().flat_map(|(_, m)| m).collect();
            groups.push(DuplicateGroup {
                classification: "name-only",
                signature_hash: String::new(),
                body_hash: None,
                shape_fingerprint: fp,
                members,
            });
        } else if body_groups.len() == 1 {
            // Same sig, same body → exact
            let (bh, members) = body_groups.into_iter().next().unwrap();
            let body_hash = if bh.is_empty() { None } else { Some(bh) };
            groups.push(DuplicateGroup {
                classification: "exact",
                signature_hash: sh,
                body_hash,
                shape_fingerprint: fp,
                members,
            });
        } else {
            // Same sig, multiple body hashes → structural (one group per body hash)
            for (bh, members) in body_groups {
                let body_hash = if bh.is_empty() { None } else { Some(bh) };
                groups.push(DuplicateGroup {
                    classification: "structural",
                    signature_hash: sh.clone(),
                    body_hash,
                    shape_fingerprint: fp.clone(),
                    members,
                });
            }
        }
    }

    // Sort: exact first, then structural, then name-only; within class by member count desc
    groups.sort_by(|a, b| {
        let class_ord = |c: &str| match c {
            "exact" => 0,
            "structural" => 1,
            _ => 2,
        };
        class_ord(a.classification)
            .cmp(&class_ord(b.classification))
            .then(b.members.len().cmp(&a.members.len()))
    });

    groups
}

/// Piece 0.12: reads the read model — see `Snapshot::symbols_by_name`.
/// Merges across every loaded workspace when `params.workspace` is `None`.
pub(crate) async fn duplicates_of_name_impl(
    state: &SharedState,
    params: DuplicatesQuery,
) -> Result<serde_json::Value, AppError> {
    let snaps = snapshots_for(state, params.workspace.as_deref())?;
    let mut symbols: Vec<Symbol> = Vec::new();
    for snap in &snaps {
        symbols.extend(snap.symbols_by_name(&params.name, params.kind.as_deref()));
    }
    let total = symbols.len();
    let groups = classify_symbols(symbols);
    let resp = DuplicatesResponse {
        name: params.name,
        workspace: params.workspace,
        groups,
        total_locations: total,
    };
    Ok(serde_json::json!(resp))
}

async fn duplicates_of_name(
    State(state): State<SharedState>,
    Query(params): Query<DuplicatesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(duplicates_of_name_impl(&state, params).await?))
}

pub(crate) async fn all_duplicates_impl(
    state: &SharedState,
    params: AllDuplicatesQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let min_count = params.min_count.unwrap_or(2);
        let name_kind_totals = reader::get_duplicate_names(
            db,
            &params.workspace,
            min_count,
            params.kind.as_deref(),
        )?;

        let mut duplicates: Vec<AllDuplicateEntry> = Vec::new();

        for (name, kind, total) in name_kind_totals {
            // Fetch symbols to classify
            let symbols = reader::get_symbols_by_name(
                db,
                &name,
                Some(&params.workspace),
                Some(&kind),
            )?;
            let groups = classify_symbols(symbols);

            let mut exact_groups = 0usize;
            let mut structural_groups = 0usize;
            let mut name_only_count = 0usize;

            for g in &groups {
                match g.classification {
                    "exact" => exact_groups += 1,
                    "structural" => structural_groups += 1,
                    _ => name_only_count += g.members.len(),
                }
            }

            duplicates.push(AllDuplicateEntry {
                name,
                kind,
                total,
                exact_groups,
                structural_groups,
                name_only_count,
            });
        }

        // Already sorted by total desc from SQL
        let resp = AllDuplicatesResponse {
            workspace: params.workspace,
            duplicates,
        };
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn all_duplicates(
    State(state): State<SharedState>,
    Query(params): Query<AllDuplicatesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(all_duplicates_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/semantic_duplicates
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct SemanticDuplicatesQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
    pub(crate) kind: Option<String>,
    /// Minimum distinct names in group (default 1).
    pub(crate) min_names: Option<usize>,
    /// Minimum total locations in group (default 2).
    pub(crate) min_locations: Option<usize>,
    /// Use loose fingerprint (ignores field names / method names). Default false.
    pub(crate) loose: Option<bool>,
    /// Minimum field_count for a group (default 0).
    pub(crate) min_fields: Option<u32>,
    /// Maximum field_count for a group (default 999999).
    pub(crate) max_fields: Option<u32>,
}

#[derive(Serialize)]
struct SemanticMember {
    name: String,
    file: String,
    line: u32,
    kind: String,
}

#[derive(Serialize)]
struct SemanticGroup {
    fingerprint: String,
    kind: String,
    shape_summary: String,
    field_count: Option<u32>,
    names_count: usize,
    total_locations: usize,
    members: Vec<SemanticMember>,
}

#[derive(Serialize)]
struct SemanticDuplicatesResponse {
    total_groups: usize,
    groups: Vec<SemanticGroup>,
}

pub(crate) async fn semantic_duplicates_impl(
    state: &SharedState,
    params: SemanticDuplicatesQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
    let min_names = params.min_names.unwrap_or(1);
    let min_locations = params.min_locations.unwrap_or(2);
    let use_loose = params.loose.unwrap_or(false);
    let min_fields = params.min_fields.unwrap_or(0);
    let max_fields = params.max_fields.unwrap_or(999_999);

    // Parse kind filter
    let kinds: Option<Vec<String>> = params.kind.as_deref().and_then(|k| {
        if k == "all" || k.is_empty() {
            None
        } else {
            Some(k.split(',').map(|s| s.trim().to_owned()).collect())
        }
    });

    let rows = reader::get_semantic_dup_rows(
        db,
        reader::SemanticDupOptions {
            workspace_id: &params.workspace,
            path_prefix: params.path_prefix.as_deref(),
            kinds: kinds.as_deref(),
            loose: use_loose,
        },
    )?;

    // Group by fingerprint
    let mut fp_to_members: std::collections::BTreeMap<String, Vec<SemanticMember>> =
        std::collections::BTreeMap::new();
    let mut fp_to_kind: HashMap<String, String> = HashMap::new();
    // Take the first non-None shape_summary and field_count per fingerprint.
    let mut fp_to_summary: HashMap<String, Option<String>> = HashMap::new();
    let mut fp_to_field_count: HashMap<String, Option<u32>> = HashMap::new();

    for row in rows {
        let member = SemanticMember {
            name: row.name,
            file: row.file_path,
            line: row.line,
            kind: row.kind.clone(),
        };
        fp_to_kind
            .entry(row.shape_fingerprint.clone())
            .or_insert(row.kind);
        fp_to_summary
            .entry(row.shape_fingerprint.clone())
            .or_insert(row.shape_summary);
        fp_to_field_count
            .entry(row.shape_fingerprint.clone())
            .or_insert(row.field_count);
        fp_to_members
            .entry(row.shape_fingerprint)
            .or_default()
            .push(member);
    }

    // Build groups, apply filters
    let mut groups: Vec<SemanticGroup> = Vec::new();

    for (fp, members) in fp_to_members {
        let total_locations = members.len();
        if total_locations < min_locations {
            continue;
        }

        let unique_names: std::collections::HashSet<&str> =
            members.iter().map(|m| m.name.as_str()).collect();
        if unique_names.len() < min_names {
            continue;
        }

        let field_count = fp_to_field_count.get(&fp).and_then(|v| *v);
        // Apply field_count filter (only when field_count is known)
        if let Some(fc) = field_count {
            if fc < min_fields || fc > max_fields {
                continue;
            }
        }

        let kind = fp_to_kind
            .get(&fp)
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned());

        let shape_summary = fp_to_summary
            .get(&fp)
            .and_then(|v| v.clone())
            .unwrap_or_default();

        let names_count = unique_names.len();

        groups.push(SemanticGroup {
            fingerprint: fp,
            kind,
            shape_summary,
            field_count,
            names_count,
            total_locations,
            members,
        });
    }

    // Sort: total_locations desc, then names_count desc
    groups.sort_by(|a, b| {
        b.total_locations
            .cmp(&a.total_locations)
            .then(b.names_count.cmp(&a.names_count))
    });

    let total_groups = groups.len();
    let resp = SemanticDuplicatesResponse {
        total_groups,
        groups,
    };
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn semantic_duplicates(
    State(state): State<SharedState>,
    Query(params): Query<SemanticDuplicatesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(semantic_duplicates_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/impls?trait=NAME&workspace=ID&path_prefix=P
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct ImplsQuery {
    #[serde(rename = "trait")]
    pub(crate) trait_name: String,
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
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
struct ImplsResponse {
    #[serde(rename = "trait")]
    trait_name: String,
    implementors: Vec<ImplementorEntry>,
    total: usize,
}

/// Piece 0.12: reads the read model — see `Snapshot::impls_of_trait`.
pub(crate) async fn impls_of_trait_impl(
    state: &SharedState,
    params: ImplsQuery,
) -> Result<serde_json::Value, AppError> {
    let snap = snapshot(state, &params.workspace)?;
    let symbols = snap.impls_of_trait(&params.trait_name, params.path_prefix.as_deref());
    let total = symbols.len();
    let implementors: Vec<ImplementorEntry> = symbols
        .into_iter()
        .map(|s| ImplementorEntry {
            type_name: s.name,
            file: s.file_path,
            line: s.line,
            impl_id: s.id,
        })
        .collect();
    let resp = ImplsResponse {
        trait_name: params.trait_name,
        implementors,
        total,
    };
    Ok(serde_json::json!(resp))
}

async fn impls_of_trait(
    State(state): State<SharedState>,
    Query(params): Query<ImplsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(impls_of_trait_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/inherent_impls?type=NAME&workspace=ID&path_prefix=P
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct InherentImplsQuery {
    #[serde(rename = "type")]
    pub(crate) type_name: String,
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
}

#[derive(Serialize)]
struct InherentImplEntry {
    file: String,
    line: u32,
    impl_id: i64,
    method_count: u32,
}

#[derive(Serialize)]
struct InherentImplsResponse {
    #[serde(rename = "type")]
    type_name: String,
    impls: Vec<InherentImplEntry>,
}

/// Piece 0.12: reads the read model — see
/// `Snapshot::inherent_impls_of_type`/`Snapshot::method_count_for_impl`.
pub(crate) async fn inherent_impls_of_type_impl(
    state: &SharedState,
    params: InherentImplsQuery,
) -> Result<serde_json::Value, AppError> {
    let snap = snapshot(state, &params.workspace)?;
    let impl_syms = snap.inherent_impls_of_type(&params.type_name, params.path_prefix.as_deref());

    let impls: Vec<InherentImplEntry> = impl_syms
        .into_iter()
        .map(|sym| {
            let count = snap.method_count_for_impl(&sym.file_path, &sym.name);
            InherentImplEntry {
                file: sym.file_path,
                line: sym.line,
                impl_id: sym.id,
                method_count: count,
            }
        })
        .collect();

    let resp = InherentImplsResponse {
        type_name: params.type_name,
        impls,
    };
    Ok(serde_json::json!(resp))
}

async fn inherent_impls_of_type(
    State(state): State<SharedState>,
    Query(params): Query<InherentImplsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(inherent_impls_of_type_impl(&state, params).await?))
}

// ---------------------------------------------------------------------------
// /ast/impl_summary?workspace=ID&path_prefix=P
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct ImplSummaryQuery {
    pub(crate) workspace: String,
    pub(crate) path_prefix: Option<String>,
}

#[derive(Serialize)]
struct TraitImplSummaryEntry {
    name: String,
    implementor_count: u32,
    implementors: Vec<String>,
}

#[derive(Serialize)]
struct ImplSummaryResponse {
    traits: Vec<TraitImplSummaryEntry>,
}

pub(crate) async fn impl_summary_impl(
    state: &SharedState,
    params: ImplSummaryQuery,
) -> Result<serde_json::Value, AppError> {
    with_db(state, move |db| {
        let rows = reader::get_impl_summary(db, &params.workspace, params.path_prefix.as_deref())?;
        let traits: Vec<TraitImplSummaryEntry> = rows
            .into_iter()
            .map(|r| TraitImplSummaryEntry {
                name: r.trait_name,
                implementor_count: r.implementor_count,
                implementors: r.implementors,
            })
            .collect();
        let resp = ImplSummaryResponse { traits };
        Ok(serde_json::json!(resp))
    })
    .await
}

async fn impl_summary(
    State(state): State<SharedState>,
    Query(params): Query<ImplSummaryQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(impl_summary_impl(&state, params).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::writer;
    use crate::index::lock::IndexLocks;
    use crate::server::state::AppState;
    use crate::server::stats::StatsCache;
    use crate::storage::{BatchConfig, BatchWriter, Db, DbConfig, DbPool, MigrationRunner};
    use crate::types::{CallEdge, FileRecord, Symbol, SymbolKind, Visibility};
    use std::sync::{Arc, RwLock};

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "m4a-ast-callers-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn test_symbol(workspace_id: &str, file_path: &str, name: &str, line: u32) -> Symbol {
        Symbol {
            id: 0,
            workspace_id: workspace_id.to_string(),
            file_path: file_path.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            visibility: Visibility::Public,
            line,
            col: 0,
            parent_name: None,
            signature: None,
            body_hash: None,
            shape_fingerprint: None,
            shape_fingerprint_loose: None,
            field_count: None,
            shape_summary: None,
            callsite_count: 0,
            type_use_count: 0,
            impl_trait: None,
        }
    }

    /// Seeds `def_count` definitions of `overloaded_name` (one per file) plus
    /// a single caller symbol `caller` (in `caller.rs`) that calls it once —
    /// enough to distinguish "the ambiguity guard refused" from "there just
    /// happen to be no callers".
    async fn seeded_state(overloaded_name: &str, def_count: u32) -> (SharedState, std::path::PathBuf) {
        let path = temp_db_path("ambiguous");
        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        db.run_migrations(MigrationRunner::new(crate::storage::migrations::legacy_schema_migrations()))
            .await
            .expect("migration applies");

        let write_db = db.clone();
        let name = overloaded_name.to_string();
        tokio::task::spawn_blocking(move || {
            write_db.write_blocking(|conn| {
                writer::upsert_file_record(
                    conn,
                    &FileRecord { workspace_id: "ws".to_string(), file_path: "caller.rs".to_string(), mtime: 0, size: 0, symbol_count: 0 },
                )?;
                let caller_id = writer::insert_symbol(conn, &test_symbol("ws", "caller.rs", "caller", 1))?;
                writer::insert_call_edge(
                    conn,
                    &CallEdge {
                        workspace_id: "ws".to_string(),
                        caller_file: "caller.rs".to_string(),
                        caller_symbol_id: caller_id,
                        callee_name: name.clone(),
                        line: 1,
                    },
                )?;

                for i in 0..def_count {
                    let file = format!("def_{i}.rs");
                    writer::upsert_file_record(
                        conn,
                        &FileRecord { workspace_id: "ws".to_string(), file_path: file.clone(), mtime: 0, size: 0, symbol_count: 0 },
                    )?;
                    writer::insert_symbol(conn, &test_symbol("ws", &file, &name, 1))?;
                }
                Ok(())
            })
        })
        .await
        .expect("join")
        .expect("seed writes succeed");

        let reader_pool = DbPool::open_with_size(&DbConfig::new(&path), 4).expect("reader pool opens");
        let writer_handle = BatchWriter::new(db, BatchConfig::default());
        let read_model = crate::model::ReadModel::new();
        crate::model::read_model::build_and_set(&reader_pool, &read_model, "ws")
            .await
            .expect("read-model snapshot builds");
        let state: SharedState = Arc::new(AppState {
            writer: writer_handle,
            reader_pool,
            data_dir: "test-data".to_string(),
            stats_cache: Arc::new(RwLock::new(StatsCache::empty())),
            index_locks: IndexLocks::new(),
            last_index: Arc::new(RwLock::new(None)),
            read_model,
            startup: crate::server::startup::StartupState::ready(),
        });
        (state, path)
    }

    #[tokio::test]
    async fn callers_of_a_name_at_the_threshold_computes_normally() {
        let (state, path) = seeded_state("common_helper", AMBIGUOUS_DEFINITION_THRESHOLD).await;

        let value = callers_of_symbol_impl(
            &state,
            SymQuery { sym: "common_helper".to_string(), workspace: Some("ws".to_string()) },
        )
        .await
        .expect("callers_of_symbol_impl succeeds");

        assert!(value.is_array(), "at-threshold definition count must NOT be refused: {value}");
        let names: Vec<&str> = value.as_array().expect("array").iter().map(|s| s["name"].as_str().unwrap_or_default()).collect();
        assert!(names.contains(&"caller"), "{names:?}");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn callers_of_a_name_over_the_threshold_is_refused_as_ambiguous() {
        let over = AMBIGUOUS_DEFINITION_THRESHOLD + 1;
        let (state, path) = seeded_state("overloaded_name", over).await;

        let value = callers_of_symbol_impl(
            &state,
            SymQuery { sym: "overloaded_name".to_string(), workspace: Some("ws".to_string()) },
        )
        .await
        .expect("callers_of_symbol_impl succeeds");

        assert_eq!(value["ambiguous"], true, "{value}");
        assert_eq!(value["definition_count"], over, "{value}");
        let message = value["message"].as_str().expect("message is a string");
        assert!(message.contains("overloaded_name"), "{message}");
        assert!(message.contains(&over.to_string()), "{message}");

        let _ = std::fs::remove_file(&path);
    }
}
