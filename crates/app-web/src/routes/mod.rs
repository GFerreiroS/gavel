//! Route table.
//!
//! Handlers here are thin: extract, call a service, render a view. Anything
//! longer than a screen belongs in `app-core` or in the cluster runtime.

mod account;
mod cluster;
mod debug;
pub(crate) mod enhancements;
pub(crate) mod gear;
mod gear_stats;
mod item;
mod jobs;
mod market;
mod pages;
mod partials;
mod reagents;
mod stream;
pub(crate) mod tooltip;
mod wow;

use app_core::Ports;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::{assets, csrf, metrics, prefs};

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
        .route("/wow/auctions", get(market::index::<E>))
        .route("/wow/consumables", get(market::page_handler::<E>))
        .route("/wow/auctions/reagents", get(reagents::page_handler::<E>))
        .route(
            "/wow/auctions/enchants",
            get(enhancements::enchants_page::<E>),
        )
        .route("/wow/auctions/gems", get(enhancements::gems_page::<E>))
        .route("/wow/auctions/gear", get(gear::page_handler::<E>))
        .route("/wow/auctions/recipes", get(gear::recipes_page::<E>))
        // One page per item level: the market is the (item, item level) pair.
        .route(
            "/wow/gear/{item_id}/{item_level}",
            get(gear_stats::stats::<E>),
        )
        // A recipe has one version of itself, so it has no item level.
        .route("/wow/recipe/{item_id}", get(gear_stats::recipe_stats::<E>))
        // Distinct prefixes so an expansion slug can never shadow an item id.
        .route("/wow/expansion/{id}", get(market::archived_page::<E>))
        .route("/wow/item/{item_id}", get(item::detail::<E>))
        .route("/wow/item/{item_id}/tooltip", get(tooltip::tooltip::<E>))
        // HTMX fragments
        .route("/partials/stats", get(partials::stats::<E>))
        .route("/partials/nodes", get(partials::nodes::<E>))
        .route("/partials/events", get(partials::events::<E>))
        .route("/partials/metrics", get(partials::metrics::<E>))
        .route("/partials/consumables", get(market::fragment::<E>))
        .route("/partials/reagents", get(reagents::fragment::<E>))
        .route(
            "/partials/enchants",
            get(enhancements::enchants_fragment::<E>),
        )
        .route("/partials/gems", get(enhancements::gems_fragment::<E>))
        .route("/partials/gear", get(gear::fragment::<E>))
        .route("/partials/recipes", get(gear::recipes_fragment::<E>))
        .route("/partials/jobs", get(partials::jobs::<E>))
        .route("/partials/jobs/{id}", get(partials::job_detail::<E>))
        .route("/wow/character", get(wow::character::<E>))
        // JSON is for scripts and future non-browser clients; the browser
        // uses the HTML fragments above.
        .route("/api/cluster", get(pages::snapshot_json::<E>))
        .route("/api/metrics", get(pages::metrics_json::<E>))
        // Live updates. The fragments above keep a slow poll as a fallback.
        .route("/events/stream", get(stream::events::<E>))
        // Container/readiness probe. No port call and therefore no database
        // dependency: it only proves the HTTP process can answer.
        .route("/healthz", get(healthz))
        // actions
        .route("/nodes/{id}/roles", post(cluster::set_role::<E>))
        .route("/account/register", post(account::register::<E>))
        .route("/account/login", post(account::login::<E>))
        .route("/account/logout", post(account::logout::<E>))
        .merge(debug_routes)
        // assets
        .route("/static/pico.css", get(assets::pico))
        .route("/static/style.css", get(assets::style))
        .route("/static/htmx.min.js", get(assets::htmx))
        .route("/static/live.js", get(assets::live))
        .route("/favicon.ico", get(assets::favicon))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(csrf::layer))
        // Resolves region+language for every request, and remembers an
        // explicit choice in a cookie on the way out.
        .layer(axum::middleware::from_fn_with_state(
            env.clone(),
            prefs::layer::<E>,
        ))
        // Outermost, so the timing covers everything below it.
        .layer(axum::middleware::from_fn_with_state(
            env.clone(),
            metrics::layer::<E>,
        ))
        // Outside the handlers, inside tracing. CSS is render-blocking and
        // Pico is 70 KB of it uncompressed, so this is what pays for it:
        // measured over the four static assets, 146 KB raw becomes 34 KB
        // gzipped. The app served 72 KB uncompressed before Pico existed, so
        // adding a framework more than halved what actually crosses the wire.
        //
        // The default predicate skips `text/event-stream`, which matters: a
        // compressed SSE body buffers, and buffered events are late events.
        .layer(CompressionLayer::new().gzip(true).br(true))
        .layer(TraceLayer::new_for_http())
        .with_state(env)
}

async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn not_found() -> Response {
    crate::error::WebError(app_core::AppError::NotFound).into_response()
}
