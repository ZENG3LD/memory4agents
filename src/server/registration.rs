//! Builds the `nemo_service::manifest::ServiceManifest` dig2memory-server
//! asserts about itself via `PUT /registry/dig2memory-server` (see
//! `bin/server.rs`'s call to `nemo_service::register::spawn`). `endpoints`
//! is the list `crate::server::routes::build_router` recorded while
//! building the real router via `nemo_service::router::DocRouter` — there
//! is exactly one place a route is described. Replaces the hand-written
//! `.nemo-service.toml`, deleted in the same commit as this module.

use nemo_service::manifest::{
    Dependencies, Endpoint, HealthInfo, Links, ManageAction, ParamSpec, ServiceManifest, ServiceMeta,
};

/// `dig2memory/CLAUDE.md`'s own one-line description, carried over verbatim
/// from the retired `.nemo-service.toml`.
const SERVICE_DESCRIPTION: &str = "AST index + dependency graph + fuzzy search server (port 18200)";

/// Build the manifest dig2memory-server asserts about itself. `endpoints`
/// is the list `crate::server::routes::build_router` recorded while
/// building the real router — passed in rather than rebuilt here so there
/// is exactly one place a route is described. `port` is the bind port
/// actually read from `Config` (`DIG2MEMORY_PORT`, default 18200), so
/// `links.local_url` can never drift from what the daemon actually bound
/// to.
///
/// No `key_ref` anywhere in this manifest: every route on this server is
/// unauthenticated (`auth = "none"`, see each router's `RouteDoc`) — there
/// is no bearer, session, or API key to name. `dig2memory-server` relies
/// entirely on binding to `127.0.0.1` for its access control.
pub fn service_manifest(endpoints: Vec<Endpoint>, port: u16) -> ServiceManifest {
    ServiceManifest {
        service: ServiceMeta {
            name: "dig2memory-server".to_string(),
            kind: "service".to_string(),
            display_name: Some("dig2memory".to_string()),
            description: Some(SERVICE_DESCRIPTION.to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            key_ref: None,
        },
        links: Links {
            public_url: None,
            local_url: Some(format!("http://127.0.0.1:{port}")),
            admin_url: None,
            docs: Some("dig2memory/README.md".to_string()),
            repo: Some("dig2memory".to_string()),
        },
        endpoints,
        manage_actions: vec![
            ManageAction {
                id: "reindex".to_string(),
                label: "Re-index nemo workspace".to_string(),
                description: Some("Trigger incremental re-index of the nemo workspace (force=false)".to_string()),
                method: "POST".to_string(),
                path: "/index".to_string(),
                auth: "none".to_string(),
                key_ref: None,
                confirm: false,
                icon: None,
                body: Some(
                    r#"{"workspace_id":"nemo","workspace_name":"nemo","root_path":"c:/src/nemo","force":false}"#
                        .to_string(),
                ),
                content_type: Some("application/json".to_string()),
                timeout_secs: Some(600),
                mode: Some("async".to_string()),
                params_schema: Vec::new(),
            },
            ManageAction {
                id: "search_symbol".to_string(),
                label: "Fuzzy search symbol".to_string(),
                description: Some("Find functions/structs/traits by name. Params: {q}".to_string()),
                method: "GET".to_string(),
                path: "/ast/search?q={q}&workspace=nemo".to_string(),
                auth: "none".to_string(),
                key_ref: None,
                confirm: false,
                icon: None,
                body: None,
                content_type: None,
                timeout_secs: Some(30),
                mode: None,
                params_schema: vec![ParamSpec {
                    name: "q".to_string(),
                    required: true,
                    example: Some("AppState".to_string()),
                    description: Some("Symbol name to fuzzy-search".to_string()),
                }],
            },
            ManageAction {
                id: "find_callers".to_string(),
                label: "Find callers of symbol".to_string(),
                description: Some("List all callers of a symbol. Params: {sym}".to_string()),
                method: "GET".to_string(),
                path: "/ast/callers?sym={sym}&workspace=nemo".to_string(),
                auth: "none".to_string(),
                key_ref: None,
                confirm: false,
                icon: None,
                body: None,
                content_type: None,
                timeout_secs: None,
                mode: None,
                params_schema: Vec::new(),
            },
            ManageAction {
                id: "resolve_symbol".to_string(),
                label: "Resolve crate owning symbol".to_string(),
                description: Some("Which crate defines this symbol. Params: {sym}".to_string()),
                method: "GET".to_string(),
                path: "/graph/resolve?sym={sym}&workspace=nemo".to_string(),
                auth: "none".to_string(),
                key_ref: None,
                confirm: false,
                icon: None,
                body: None,
                content_type: None,
                timeout_secs: None,
                mode: None,
                params_schema: Vec::new(),
            },
            ManageAction {
                id: "impact_analysis".to_string(),
                label: "Impact analysis for file".to_string(),
                description: Some("What breaks if this file changes. Params: {file}".to_string()),
                method: "GET".to_string(),
                path: "/graph/impact?file={file}&workspace=nemo".to_string(),
                auth: "none".to_string(),
                key_ref: None,
                confirm: false,
                icon: None,
                body: None,
                content_type: None,
                timeout_secs: Some(300),
                mode: Some("async".to_string()),
                params_schema: vec![ParamSpec {
                    name: "file".to_string(),
                    required: true,
                    example: Some("mylittlechart/crates/indicators/src/bar_indicators/mod.rs".to_string()),
                    description: Some("Workspace-relative file path".to_string()),
                }],
            },
            ManageAction {
                id: "hotspots".to_string(),
                label: "Most-connected modules".to_string(),
                description: Some("Hotspot modules in the graph (no params)".to_string()),
                method: "GET".to_string(),
                path: "/graph/hotspots?workspace=nemo".to_string(),
                auth: "none".to_string(),
                key_ref: None,
                confirm: false,
                icon: None,
                body: None,
                content_type: None,
                timeout_secs: None,
                mode: None,
                params_schema: Vec::new(),
            },
            ManageAction {
                id: "crate_tree".to_string(),
                label: "Crate dependency tree".to_string(),
                description: Some("Full crate dependency graph (no params)".to_string()),
                method: "GET".to_string(),
                path: "/graph/crates?workspace=nemo".to_string(),
                auth: "none".to_string(),
                key_ref: None,
                confirm: false,
                icon: None,
                body: None,
                content_type: None,
                timeout_secs: None,
                mode: None,
                params_schema: Vec::new(),
            },
        ],
        dependencies: Dependencies::default(),
        health: Some(HealthInfo {
            endpoint: "/health".to_string(),
            expected_status: 200,
            interval_secs: 30,
        }),
    }
}
