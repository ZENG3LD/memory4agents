//! `POST /mcp` — the MCP (Model Context Protocol) door onto the SAME
//! `/ast/*`, `/graph/*`, `/ast/*` (usage/dead-code family) and `/index`
//! surface the plain HTTP routes serve, built on the shared
//! `mcp_service4agent::mcp::McpServer` (crate `mcp-service4agent`) instead of
//! a hand-written JSON-RPC implementation — see
//! `docs/memory4agent/plans/2026-09-23-memory4agent-plan.md`, phase 1.
//! Every tool below dispatches into the exact `*_impl` function its HTTP
//! sibling in `ast`/`graph`/`usage`/`index` already calls — one
//! implementation, two doors (`memory4agent/CLAUDE.md`).
//!
//! # Tool budget — fixed for every later phase
//!
//! Exactly 8 tools, allocated once here and never grown (`Budget::WORKSPACE`:
//! at most 8 tools, `tools/list` at most 8 KB compact, no tool over 1.5 KB).
//! `eight_tools_within_the_workspace_mcp_budget` below fails the build if
//! that creeps back up. Phase 2/3/4/5 upgrade a tool's *handler*, never its
//! schema shape — see the plan's cross-phase MCP tool ledger.
//!
//! # No caller-identity seam
//!
//! Unlike `mail4agent`, every route on this server is `auth: "none"` — access
//! control is "binds to 127.0.0.1 only" (`server/registration.rs`). So this
//! module needs none of `mail4agent::routes::mcp`'s `resolve_caller_middleware`
//! seam: `McpServer::into_router()` mounts with no auth layer, and
//! `route_docs()`'s hardcoded `auth: "bearer"` is overwritten to `"none"` by
//! `server::routes::build_router` before folding into the manifest.
//!
//! # `mem_cleanup`'s `arg` field
//!
//! The plan's schema table keeps `mem_cleanup` terse
//! (`{workspace?, what, path_prefix?, limit?}`), but three of its 13 `what`
//! kinds need one more identifying string the underlying REST route already
//! takes as a required query param: `cluster` and `find_twins` need a seed
//! symbol name, `dead_in_group` needs a shape fingerprint, `role_groups`
//! optionally takes a token filter. Rather than growing the schema with
//! four rarely-used fields (over budget for little gain), one optional
//! `arg` string carries whichever of those a given `what` needs — the
//! *rule* ("what=\"cluster\" needs arg=<seed name>") is taught by a named
//! refusal the first time an agent omits it, per L5, not spelled out in the
//! schema's prose.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::state::SharedState;
use crate::IndexRequest;
use mcp_service4agent::mcp::{McpServer, Tool, ToolOutcome};

use super::ast;
use super::graph;
use super::health::health_snapshot;
use super::index::spawn_reindex_job;
use super::usage;

const SERVER_NAME: &str = "memory4agent";

// ---------------------------------------------------------------------------
// Shared result rendering
// ---------------------------------------------------------------------------

/// Renders a `*_impl` outcome (already-compact `serde_json::Value` or a
/// [`crate::server::state::AppError`]) as a [`ToolOutcome`] — `Ok` is
/// [`ToolOutcome::ok`]; `Err` is STILL [`ToolOutcome::ok`]-shaped
/// (`isError: true`, never a JSON-RPC error) carrying the error text, same
/// split `mail4agent::routes::mcp::tool_result` documents.
fn tool_result(outcome: Result<Value, crate::server::state::AppError>) -> ToolOutcome {
    match outcome {
        Ok(value) => ToolOutcome::ok(value),
        Err(err) => ToolOutcome::error(format!("{err:?}")),
    }
}

fn invalid_args(field: &str, err: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::invalid_argument(field, err)
}

// ---------------------------------------------------------------------------
// mem_search
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    scope: String,
    workspace: Option<String>,
    limit: Option<u32>,
}

fn tool_search() -> Tool {
    Tool::new(
        "mem_search",
        "Fuzzy search. scope=\"symbol\"|\"code\" is name-based trigram search over the AST index; \
         scope=\"docs\" is not implemented until phase 5.",
        json!({
            "type": "object",
            "required": ["query", "scope"],
            "additionalProperties": false,
            "properties": {
                "query": {"type": "string"},
                "scope": {"type": "string", "enum": ["symbol", "code", "docs"]},
                "workspace": {"type": ["string", "null"]},
                "limit": {"type": ["integer", "null"], "minimum": 1, "maximum": 50}
            }
        }),
    )
}

async fn dispatch_search(state: &SharedState, args: Value) -> ToolOutcome {
    let args: SearchArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return invalid_args("arguments", e),
    };
    match args.scope.as_str() {
        "symbol" | "code" => {
            let params = ast::SearchQuery { q: args.query, workspace: args.workspace, limit: args.limit };
            tool_result(ast::search_symbols_impl(state, params).await)
        }
        "docs" => ToolOutcome::error("scope=\"docs\" is not implemented until phase 5 (hybrid BM25+vector retrieval)"),
        other => invalid_args("scope", format!("unknown scope {other:?}, expected symbol|code|docs")),
    }
}

// ---------------------------------------------------------------------------
// mem_symbol
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SymbolArgs {
    name: String,
    workspace: Option<String>,
    what: String,
    depth: Option<u32>,
}

fn tool_symbol() -> Tool {
    Tool::new(
        "mem_symbol",
        "Look up a symbol by name. what=definition (all defining locations, classified by \
         signature/body match), callers, impact (BFS over callers, depth<=4), type_usage \
         (who uses this name as a type), implementors (trait implementors + inherent impls).",
        json!({
            "type": "object",
            "required": ["name", "what"],
            "additionalProperties": false,
            "properties": {
                "name": {"type": "string"},
                "workspace": {"type": ["string", "null"]},
                "what": {"type": "string", "enum": ["definition", "callers", "impact", "type_usage", "implementors"]},
                "depth": {"type": ["integer", "null"], "minimum": 1, "maximum": 4}
            }
        }),
    )
}

async fn dispatch_symbol(state: &SharedState, args: Value) -> ToolOutcome {
    let args: SymbolArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return invalid_args("arguments", e),
    };
    let workspace = args.workspace;
    match args.what.as_str() {
        "definition" => {
            let params = ast::DuplicatesQuery { name: args.name, workspace, kind: None };
            tool_result(ast::duplicates_of_name_impl(state, params).await)
        }
        "callers" => {
            let params = ast::SymQuery { sym: args.name, workspace };
            match ast::callers_of_symbol_impl(state, params).await {
                Ok(value) => {
                    // Explicit truncation marker for the MCP text output
                    // (piece 0.12, item 4): a bare capped array gives an
                    // agent no signal that more callers exist beyond
                    // `model::MAX_CALLERS` — HTTP's `/ast/callers` keeps its
                    // existing bare-array shape unchanged; this wrapping is
                    // MCP-only. The ambiguous-refusal shape (an object, not
                    // an array) is untouched — `as_array()` is `None` for it.
                    if let Some(arr) = value.as_array() {
                        if arr.len() >= crate::model::MAX_CALLERS {
                            return ToolOutcome::ok(json!({
                                "callers": value,
                                "truncated": true,
                                "truncated_at": crate::model::MAX_CALLERS,
                            }));
                        }
                    }
                    ToolOutcome::ok(value)
                }
                Err(e) => ToolOutcome::error(format!("{e:?}")),
            }
        }
        "impact" => {
            let params = graph::SymbolImpactQuery { sym: args.name, workspace, depth: args.depth };
            tool_result(graph::impact_analysis_symbol_impl(state, params).await)
        }
        "type_usage" => {
            let params = usage::TypeUsageQuery {
                name: args.name,
                workspace: workspace.unwrap_or_default(),
                path_prefix: None,
                breakdown: Some(true),
            };
            tool_result(usage::type_usage_impl(state, params).await)
        }
        "implementors" => {
            let workspace = workspace.unwrap_or_default();
            let as_trait = ast::impls_of_trait_impl(
                state,
                ast::ImplsQuery { trait_name: args.name.clone(), workspace: workspace.clone(), path_prefix: None },
            )
            .await;
            let as_type = ast::inherent_impls_of_type_impl(
                state,
                ast::InherentImplsQuery { type_name: args.name, workspace, path_prefix: None },
            )
            .await;
            match (as_trait, as_type) {
                (Ok(t), Ok(i)) => ToolOutcome::ok(json!({ "as_trait": t, "as_type": i })),
                (Err(e), _) | (_, Err(e)) => ToolOutcome::error(format!("{e:?}")),
            }
        }
        other => invalid_args("what", format!("unknown what {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// mem_file
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct FileArgs {
    file: String,
    workspace: Option<String>,
    what: String,
}

fn tool_file() -> Tool {
    Tool::new(
        "mem_file",
        "Look up a file. what=symbols (all symbols defined in it), deps (its own dependencies), \
         impact (BFS over reverse file dependencies — what breaks if this file changes).",
        json!({
            "type": "object",
            "required": ["file", "what"],
            "additionalProperties": false,
            "properties": {
                "file": {"type": "string"},
                "workspace": {"type": ["string", "null"]},
                "what": {"type": "string", "enum": ["symbols", "deps", "impact"]}
            }
        }),
    )
}

async fn dispatch_file(state: &SharedState, args: Value) -> ToolOutcome {
    let args: FileArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return invalid_args("arguments", e),
    };
    match args.what.as_str() {
        "symbols" => {
            let params = ast::FileQuery { file: args.file, workspace: args.workspace };
            tool_result(ast::symbols_in_file_impl(state, params).await)
        }
        "deps" => {
            let params = ast::FileQuery { file: args.file, workspace: args.workspace };
            tool_result(ast::file_deps_impl(state, params).await)
        }
        "impact" => {
            let params = graph::FileQuery { file: args.file, workspace: args.workspace };
            tool_result(graph::impact_analysis_impl(state, params).await)
        }
        other => invalid_args("what", format!("unknown what {other:?}, expected symbols|deps|impact")),
    }
}

// ---------------------------------------------------------------------------
// mem_repo_map
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RepoMapArgs {
    workspace: Option<String>,
    #[serde(default)]
    focus: Vec<String>,
    limit: Option<u32>,
}

fn tool_repo_map() -> Tool {
    Tool::new(
        "mem_repo_map",
        "Workspace overview: most-connected modules (hotspots) plus the crate dependency graph. \
         Degree-based in phase 1; focus is accepted but not yet used to bias ranking — \
         personalized PageRank lands in phase 3.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "workspace": {"type": ["string", "null"]},
                "focus": {"type": "array", "maxItems": 10, "items": {"type": "string"}},
                "limit": {"type": ["integer", "null"], "minimum": 1, "maximum": 50}
            }
        }),
    )
}

async fn dispatch_repo_map(state: &SharedState, args: Value) -> ToolOutcome {
    let args: RepoMapArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return invalid_args("arguments", e),
    };
    let hotspots = graph::hotspots_impl(
        state,
        graph::HotspotsQuery { workspace: args.workspace.clone(), limit: args.limit },
    )
    .await;
    let crates = graph::list_crates_impl(state, graph::WorkspaceQuery { workspace: args.workspace }).await;
    match (hotspots, crates) {
        // `focus` is accepted (schema-required for a stable phase-3 upgrade,
        // per the cross-phase MCP tool ledger) but not yet used to bias
        // ranking — echoed back so a caller can see it round-tripped rather
        // than silently swallowed, until personalized PageRank (phase 3)
        // reads it for real.
        (Ok(h), Ok(c)) => ToolOutcome::ok(json!({ "hotspots": h, "crates": c, "focus_reserved_for_phase_3": args.focus })),
        (Err(e), _) | (_, Err(e)) => ToolOutcome::error(format!("{e:?}")),
    }
}

// ---------------------------------------------------------------------------
// mem_cleanup — see this module's own doc comment for the `arg` field.
// ---------------------------------------------------------------------------

const CLEANUP_KINDS: &[&str] = &[
    "cluster",
    "parallel_systems",
    "dead_in_group",
    "dead_types",
    "usage_hotspots",
    "orphans",
    "role_groups",
    "find_twins",
    "workspace_summary",
    "cleanup_plan",
    "duplicates",
    "all_duplicates",
    "semantic_duplicates",
];

#[derive(Deserialize)]
struct CleanupArgs {
    workspace: Option<String>,
    what: String,
    path_prefix: Option<String>,
    limit: Option<usize>,
    arg: Option<String>,
}

fn tool_cleanup() -> Tool {
    Tool::new(
        "mem_cleanup",
        "Dedup/dead-code analysis. what selects one of 13 kinds: cluster/find_twins (arg=seed \
         name, required), dead_in_group (arg=fingerprint, required), duplicates (arg=name, \
         required), role_groups (arg=token, optional), parallel_systems/dead_types/\
         usage_hotspots/orphans/workspace_summary/cleanup_plan/all_duplicates/\
         semantic_duplicates (arg unused).",
        json!({
            "type": "object",
            "required": ["what"],
            "additionalProperties": false,
            "properties": {
                "workspace": {"type": ["string", "null"]},
                "what": {"type": "string", "enum": CLEANUP_KINDS},
                "path_prefix": {"type": ["string", "null"]},
                "limit": {"type": ["integer", "null"], "minimum": 1, "maximum": 50},
                "arg": {"type": ["string", "null"], "description": "seed/fingerprint/name/token, per what — see description"}
            }
        }),
    )
}

async fn dispatch_cleanup(state: &SharedState, args: Value) -> ToolOutcome {
    let args: CleanupArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return invalid_args("arguments", e),
    };
    let workspace = args.workspace.unwrap_or_default();
    let path_prefix = args.path_prefix;

    macro_rules! require_arg {
        ($rule:literal) => {
            match args.arg {
                Some(a) if !a.is_empty() => a,
                _ => return invalid_args("arg", concat!("what=", $rule)),
            }
        };
    }

    match args.what.as_str() {
        "cluster" => {
            let seed = require_arg!("\"cluster\" requires arg = seed symbol name");
            let params = usage::ClusterQuery { seed, workspace, depth: None, path_prefix, include_dead: Some(true) };
            tool_result(usage::cluster_impl(state, params).await)
        }
        "parallel_systems" => {
            let params = usage::ParallelSystemsQuery { workspace, path_prefix, min_size: None, mode: None };
            tool_result(usage::parallel_systems_impl(state, params).await)
        }
        "dead_in_group" => {
            let fingerprint = require_arg!("\"dead_in_group\" requires arg = shape fingerprint");
            let params = usage::DeadInGroupQuery { fingerprint, workspace };
            tool_result(usage::dead_in_group_impl(state, params).await)
        }
        "dead_types" => {
            let params = usage::DeadTypesQuery { workspace, path_prefix, kind: None };
            tool_result(usage::dead_types_impl(state, params).await)
        }
        "usage_hotspots" => {
            let params = usage::UsageHotspotsQuery { workspace, path_prefix, top: args.limit.map(|l| l as u32) };
            tool_result(usage::usage_hotspots_impl(state, params).await)
        }
        "orphans" => {
            let params = usage::OrphansQuery { workspace, path_prefix, kind: None, exclude_pub_use: Some(true) };
            tool_result(usage::orphans_impl(state, params).await)
        }
        "role_groups" => {
            let params = usage::RoleGroupsQuery {
                workspace,
                path_prefix,
                token: args.arg,
                min_size: None,
                kind: None,
                use_synonyms: Some(true),
            };
            tool_result(usage::role_groups_impl(state, params).await)
        }
        "find_twins" => {
            let seed = require_arg!("\"find_twins\" requires arg = seed symbol name");
            let params = usage::FindTwinsQuery { seed, workspace, path_prefix };
            tool_result(usage::find_twins_impl(state, params).await)
        }
        "workspace_summary" => {
            let params = usage::WorkspaceSummaryQuery { workspace, path_prefix };
            tool_result(usage::workspace_summary_impl(state, params).await)
        }
        "cleanup_plan" => {
            let params = usage::CleanupPlanQuery { workspace, path_prefix, limit: args.limit };
            tool_result(usage::cleanup_plan_impl(state, params).await)
        }
        "duplicates" => {
            let name = require_arg!("\"duplicates\" requires arg = symbol name");
            let params = ast::DuplicatesQuery { name, workspace: Some(workspace), kind: None };
            tool_result(ast::duplicates_of_name_impl(state, params).await)
        }
        "all_duplicates" => {
            let params = ast::AllDuplicatesQuery { workspace, min_count: None, kind: None };
            tool_result(ast::all_duplicates_impl(state, params).await)
        }
        "semantic_duplicates" => {
            let params = ast::SemanticDuplicatesQuery {
                workspace,
                path_prefix,
                kind: None,
                min_names: None,
                min_locations: None,
                loose: None,
                min_fields: None,
                max_fields: None,
            };
            tool_result(ast::semantic_duplicates_impl(state, params).await)
        }
        other => invalid_args("what", format!("unknown what {other:?}, expected one of {CLEANUP_KINDS:?}")),
    }
}

// ---------------------------------------------------------------------------
// mem_index
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct IndexArgs {
    action: String,
    workspace_id: Option<String>,
    root_path: Option<String>,
    #[serde(default)]
    force: bool,
}

fn tool_index() -> Tool {
    Tool::new(
        "mem_index",
        "action=\"reindex\" starts a background reindex (workspace_id and root_path required) \
         and returns immediately with a job-started ack; action=\"status\" reads the last \
         reindex job's outcome. Unlike POST /index, reindex here never blocks the call.",
        json!({
            "type": "object",
            "required": ["action"],
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["reindex", "status"]},
                "workspace_id": {"type": ["string", "null"]},
                "root_path": {"type": ["string", "null"]},
                "force": {"type": "boolean"}
            }
        }),
    )
}

async fn dispatch_index(state: &SharedState, args: Value) -> ToolOutcome {
    let args: IndexArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return invalid_args("arguments", e),
    };
    match args.action.as_str() {
        "reindex" => {
            let Some(workspace_id) = args.workspace_id else {
                return invalid_args("workspace_id", "action=\"reindex\" requires workspace_id");
            };
            let Some(root_path) = args.root_path else {
                return invalid_args("root_path", "action=\"reindex\" requires root_path");
            };
            let req = IndexRequest {
                workspace_id: workspace_id.clone(),
                root_path,
                workspace_name: workspace_id.clone(),
                force: args.force,
            };
            spawn_reindex_job(state.clone(), req);
            ToolOutcome::ok(json!({ "status": "started", "workspace_id": workspace_id }))
        }
        "status" => {
            let job = state.last_index.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            let stats = health_snapshot(state);
            ToolOutcome::ok(json!({ "job": job, "stats": stats }))
        }
        other => invalid_args("action", format!("unknown action {other:?}, expected reindex|status")),
    }
}

// ---------------------------------------------------------------------------
// mem_decide — slot reserved for phase 4.
// ---------------------------------------------------------------------------

fn tool_decide() -> Tool {
    Tool::new(
        "mem_decide",
        "Reserved for the phase 4 decision log (record/query why-we-did-X entries). Refuses \
         every call until then.",
        json!({
            "type": "object",
            "required": ["action"],
            "additionalProperties": true,
            "properties": {
                "action": {"type": "string", "enum": ["record", "query"]}
            }
        }),
    )
}

async fn dispatch_decide(_state: &SharedState, _args: Value) -> ToolOutcome {
    ToolOutcome::error("mem_decide is not implemented yet — the flat decision log lands in phase 4")
}

// ---------------------------------------------------------------------------
// mem_health
// ---------------------------------------------------------------------------

fn tool_health() -> Tool {
    Tool::new(
        "mem_health",
        "Liveness snapshot: cached DB stats (workspace/symbol/file counts), never queries the \
         database directly.",
        json!({ "type": "object", "additionalProperties": false, "properties": {} }),
    )
}

async fn dispatch_health(state: &SharedState) -> ToolOutcome {
    ToolOutcome::ok(json!(health_snapshot(state)))
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Builds the 8-tool MCP server. `server::routes::build_router` calls
/// [`McpServer::route_docs`] (borrows, overwriting `auth` to `"none"`)
/// before [`McpServer::into_router`] (consumes) — see this module's own doc
/// comment, "No caller-identity seam".
pub fn build() -> McpServer<SharedState> {
    McpServer::<SharedState>::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
        .tool(tool_search(), |state, _ctx, args| async move { dispatch_search(&state, args).await })
        .tool(tool_symbol(), |state, _ctx, args| async move { dispatch_symbol(&state, args).await })
        .tool(tool_file(), |state, _ctx, args| async move { dispatch_file(&state, args).await })
        .tool(tool_repo_map(), |state, _ctx, args| async move { dispatch_repo_map(&state, args).await })
        .tool(tool_cleanup(), |state, _ctx, args| async move { dispatch_cleanup(&state, args).await })
        .tool(tool_index(), |state, _ctx, args| async move { dispatch_index(&state, args).await })
        .tool(tool_decide(), |state, _ctx, args| async move { dispatch_decide(&state, args).await })
        .tool(tool_health(), |state, _ctx, _args| async move { dispatch_health(&state).await })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use mcp_service4agent::mcp::Budget;
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt;

    use crate::db::writer;
    use crate::index::lock::IndexLocks;
    use crate::server::state::AppState;
    use crate::server::stats::StatsCache;
    use crate::storage::{BatchConfig, BatchWriter, Db, DbConfig, DbPool, MigrationRunner};
    use crate::types::{CallEdge, FileRecord, Symbol, SymbolKind, Visibility};

    /// L5's own gate: at most 8 tools, `tools/list` at most 8 KB, no tool
    /// over 1.5 KB.
    #[test]
    fn eight_tools_within_the_workspace_mcp_budget() {
        let server = build();
        if let Err(report) = server.check_budget(Budget::WORKSPACE) {
            panic!("workspace MCP budget exceeded:\n{report}");
        }
    }

    #[test]
    fn tools_list_names_the_eight_tools_in_order() {
        let server = build();
        let tools = server.tools_list_json();
        let names: Vec<&str> = tools["tools"]
            .as_array()
            .expect("tools is an array")
            .iter()
            .map(|t| t["name"].as_str().expect("name is a string"))
            .collect();
        assert_eq!(
            names,
            vec!["mem_search", "mem_symbol", "mem_file", "mem_repo_map", "mem_cleanup", "mem_index", "mem_decide", "mem_health"]
        );
    }

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "m4a-mcp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock after epoch").as_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn test_symbol(workspace_id: &str, file_path: &str, name: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            id: 0,
            workspace_id: workspace_id.to_string(),
            file_path: file_path.to_string(),
            name: name.to_string(),
            kind,
            visibility: Visibility::Public,
            line: 1,
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

    /// A small fixture: workspace "ws" with `a` (a.rs, function) and `b`
    /// (b.rs, function) calling `a` — enough to exercise every tool's
    /// dispatch path against real data instead of an empty database.
    async fn fixture_state() -> (SharedState, std::path::PathBuf) {
        let path = temp_db_path("fixture");
        let db = Db::open(&DbConfig::new(&path)).expect("writer db opens");
        db.run_migrations(MigrationRunner::new(crate::storage::migrations::legacy_schema_migrations()))
            .await
            .expect("migration applies");

        let write_db = db.clone();
        tokio::task::spawn_blocking(move || {
            write_db.write_blocking(|conn| {
                for file in ["a.rs", "b.rs"] {
                    writer::upsert_file_record(
                        conn,
                        &FileRecord { workspace_id: "ws".to_string(), file_path: file.to_string(), mtime: 0, size: 0, symbol_count: 0 },
                    )?;
                }
                writer::insert_symbol(conn, &test_symbol("ws", "a.rs", "a", SymbolKind::Function))?;
                let b_id = writer::insert_symbol(conn, &test_symbol("ws", "b.rs", "b", SymbolKind::Function))?;
                writer::insert_call_edge(
                    conn,
                    &CallEdge { workspace_id: "ws".to_string(), caller_file: "b.rs".to_string(), caller_symbol_id: b_id, callee_name: "a".to_string(), line: 1 },
                )?;
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

    fn test_router(state: SharedState) -> Router<()> {
        build().into_router().with_state(state)
    }

    async fn call(router: Router<()>, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).expect("test body serialises")))
            .expect("test request builds");
        let response = router.oneshot(request).await.expect("router never returns Err");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.expect("response body reads");
        let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).expect("response body is JSON") };
        (status, value)
    }

    fn tool_call(name: &str, arguments: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } })
    }

    fn tool_text(resp: &Value) -> Value {
        let text = resp["result"]["content"][0]["text"].as_str().expect("content[0].text is a string");
        serde_json::from_str(text).expect("tool text is compact JSON")
    }

    #[tokio::test]
    async fn mem_search_round_trips_over_the_mcp_route() {
        let (state, path) = fixture_state().await;
        let (status, resp) = call(test_router(state), tool_call("mem_search", json!({"query": "a", "scope": "symbol", "workspace": "ws"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_search_docs_scope_refuses_named() {
        let (state, path) = fixture_state().await;
        let (_, resp) = call(test_router(state), tool_call("mem_search", json!({"query": "a", "scope": "docs"}))).await;
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().expect("text is a string");
        assert!(text.contains("phase 5"), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_symbol_callers_finds_the_seeded_caller() {
        let (state, path) = fixture_state().await;
        let (status, resp) =
            call(test_router(state), tool_call("mem_symbol", json!({"name": "a", "workspace": "ws", "what": "callers"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let body = tool_text(&resp);
        let names: Vec<&str> = body.as_array().expect("callers array").iter().map(|s| s["name"].as_str().unwrap_or_default()).collect();
        assert!(names.contains(&"b"), "{names:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_file_symbols_lists_the_seeded_symbol() {
        let (state, path) = fixture_state().await;
        let (_, resp) = call(test_router(state), tool_call("mem_file", json!({"file": "a.rs", "workspace": "ws", "what": "symbols"}))).await;
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let body = tool_text(&resp);
        let names: Vec<&str> = body.as_array().expect("symbols array").iter().map(|s| s["name"].as_str().unwrap_or_default()).collect();
        assert!(names.contains(&"a"), "{names:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_repo_map_returns_hotspots_and_crates() {
        let (state, path) = fixture_state().await;
        let (_, resp) = call(test_router(state), tool_call("mem_repo_map", json!({"workspace": "ws"}))).await;
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let body = tool_text(&resp);
        assert!(body.get("hotspots").is_some(), "{body}");
        assert!(body.get("crates").is_some(), "{body}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_cleanup_requires_arg_for_cluster() {
        let (state, path) = fixture_state().await;
        let (_, resp) = call(test_router(state), tool_call("mem_cleanup", json!({"workspace": "ws", "what": "cluster"}))).await;
        assert_eq!(resp["result"]["isError"], true, "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text is a string");
        assert!(text.contains("cluster"), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_cleanup_dead_types_round_trips() {
        let (state, path) = fixture_state().await;
        let (status, resp) = call(test_router(state), tool_call("mem_cleanup", json!({"workspace": "ws", "what": "dead_types"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_index_reindex_then_status_reports_the_job() {
        let (state, path) = fixture_state().await;
        let root_path = path.parent().expect("temp path has a parent").to_string_lossy().to_string();
        let (status, resp) = call(
            test_router(state.clone()),
            tool_call("mem_index", json!({"action": "reindex", "workspace_id": "ws2", "root_path": root_path})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        assert_eq!(tool_text(&resp)["status"], "started");

        // The background job may still be running or already done — either
        // way `state.last_index` must be populated once `spawn_reindex_job`
        // has had a chance to run.
        for _ in 0..50 {
            if state.last_index.read().unwrap_or_else(|p| p.into_inner()).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let (status, resp) = call(test_router(state), tool_call("mem_index", json!({"action": "status"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        assert!(tool_text(&resp)["job"].is_object(), "{resp}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_decide_refuses_as_not_implemented() {
        let (state, path) = fixture_state().await;
        let (status, resp) = call(test_router(state), tool_call("mem_decide", json!({"action": "record", "summary": "x"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["result"]["isError"], true, "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text is a string");
        assert!(text.contains("not implemented"), "{text}");
        assert!(text.contains("phase 4"), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mem_health_reports_ok() {
        let (state, path) = fixture_state().await;
        let (status, resp) = call(test_router(state), tool_call("mem_health", json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        assert_eq!(tool_text(&resp)["ok"], true);
        let _ = std::fs::remove_file(&path);
    }
}
