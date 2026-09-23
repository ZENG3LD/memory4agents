use axum::Router;
use nemo_service::manifest::Endpoint;
use crate::server::state::SharedState;

mod ast;
mod graph;
mod health;
mod index;
mod usage;

/// Builds the full HTTP route table plus the [`Endpoint`] list recorded
/// while doing so, via `nemo_service::router::DocRouter` — the same list
/// `bin/server.rs` hands to `nemo_service::register::spawn` (through
/// `crate::server::registration::service_manifest`) to describe this
/// service truthfully. There is exactly one place a route is described:
/// here, at the same call that wires it into the real router.
pub fn build_router() -> (Router<SharedState>, Vec<Endpoint>) {
    health::router()
        .merge(index::router())
        .merge(ast::router())
        .merge(graph::router())
        .merge(usage::router())
        .into_parts()
}
