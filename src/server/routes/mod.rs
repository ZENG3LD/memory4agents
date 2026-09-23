use axum::Router;
use nemo_service::manifest::Endpoint;
use crate::server::state::SharedState;

mod ast;
mod graph;
mod health;
mod index;
mod mcp;
mod usage;

/// Builds the full HTTP route table plus the [`Endpoint`] list recorded
/// while doing so, via `nemo_service::router::DocRouter` — the same list
/// `bin/server.rs` hands to `nemo_service::register::spawn` (through
/// `crate::server::registration::service_manifest`) to describe this
/// service truthfully. There is exactly one place a route is described:
/// here, at the same call that wires it into the real router.
///
/// The MCP server (`mcp::build()`) is merged in the same way: its own
/// `route_docs()` two entries are folded into the returned `Vec<Endpoint>`
/// with `auth` overwritten from the shared library's default `"bearer"` to
/// `"none"` — memory4agent has no auth anywhere (binds to 127.0.0.1 only,
/// see `server::registration`), so a manifest entry claiming `bearer` here
/// would be a lie about this server, not a fact about MCP.
pub fn build_router() -> (Router<SharedState>, Vec<Endpoint>) {
    let (router, mut endpoints) = health::router()
        .merge(index::router())
        .merge(ast::router())
        .merge(graph::router())
        .merge(usage::router())
        .into_parts();

    let mcp_server = mcp::build();
    let mut mcp_endpoints = mcp_server.route_docs();
    for endpoint in &mut mcp_endpoints {
        endpoint.auth = "none".to_string();
    }
    endpoints.extend(mcp_endpoints);

    let router = router.merge(mcp_server.into_router());

    (router, endpoints)
}
