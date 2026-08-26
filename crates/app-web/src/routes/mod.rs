//! Route table.
//!
//! Handlers here are thin: extract, call a service, render a view. Anything
//! longer than a screen belongs in `app-core` or in the cluster runtime.

mod account;
mod cluster;
mod debug;
mod item;
mod jobs;
mod market;
mod pages;
mod partials;
mod stream;
mod wow;

use app_core::Ports;
use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::{assets, csrf, metrics};

/// Build the application router.
///
/// Generic over the port bundle, so this function never names SQLite, the
/// Tokio-task cluster or Raider.IO.
pub fn router<E: Ports>(env: E) -> Router {
    let debug_routes = if env.config().debug_controls {
        Router::new()
            .route("/debug/nodes/{id}/stop", post(debug::stop_node::<E>))
            .route("/debug/nodes/{id}/start", post(debug::start_node::<E>))
            .route(
                "/debug/nodes/{id}/heartbeat",
                post(debug::toggle_heartbeat::<E>),
            )
            .route("/debug/nodes/{id}/fail", post(debug::inject_failure::<E>))
            .route("/debug/nodes/{id}/delay", post(debug::set_delay::<E>))
    } else {
        Router::new()
    };

    Router::new()
        // pages
        .route("/", get(pages::dashboard::<E>))
        .route("/cluster", get(pages::cluster::<E>))
        .route("/nodes", get(pages::nodes::<E>))
        .route("/jobs", get(pages::jobs::<E>).post(jobs::submit::<E>))
        .route("/jobs/{id}", get(pages::job_detail::<E>))
        .route("/account", get(pages::account::<E>))
        .route("/wow", get(pages::wow::<E>))
        .route("/wow/consumables", get(market::page_handler::<E>))
        // Distinct prefixes so an expansion slug can never shadow an item id.
        .route("/wow/expansion/{id}", get(market::archived_page::<E>))
        .route("/wow/item/{item_id}", get(item::detail::<E>))
        // HTMX fragments
        .route("/partials/stats", get(partials::stats::<E>))
        .route("/partials/nodes", get(partials::nodes::<E>))
        .route("/partials/events", get(partials::events::<E>))
        .route("/partials/metrics", get(partials::metrics::<E>))
        .route("/partials/consumables", get(market::fragment::<E>))
        .route("/partials/jobs", get(partials::jobs::<E>))
        .route("/partials/jobs/{id}", get(partials::job_detail::<E>))
        .route("/wow/character", get(wow::character::<E>))
        // JSON is for scripts and future non-browser clients; the browser
        // uses the HTML fragments above (CLAUDE.md 32).
        .route("/api/cluster", get(pages::snapshot_json::<E>))
        .route("/api/metrics", get(pages::metrics_json::<E>))
        // Live updates. The fragments above keep a slow poll as a fallback.
        .route("/events/stream", get(stream::events::<E>))
        // actions
        .route("/nodes/{id}/roles", post(cluster::set_role::<E>))
        .route("/account/register", post(account::register::<E>))
        .route("/account/login", post(account::login::<E>))
        .route("/account/logout", post(account::logout::<E>))
        .merge(debug_routes)
        // assets
        .route("/static/style.css", get(assets::style))
        .route("/static/htmx.min.js", get(assets::htmx))
        .route("/static/live.js", get(assets::live))
        .route("/favicon.ico", get(assets::favicon))
        .layer(axum::middleware::from_fn(csrf::layer))
        // Outermost, so the timing covers everything below it.
        .layer(axum::middleware::from_fn_with_state(
            env.clone(),
            metrics::layer::<E>,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(env)
}
