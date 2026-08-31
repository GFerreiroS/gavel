//! Route table.
//!
//! Handlers here are thin: extract, call a service, render a view. Anything
//! longer than a screen belongs in `app-core` or in the cluster runtime.

mod account;
pub(crate) mod admin;
mod alerts;
mod archive;
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
mod realms;
mod stream;
pub(crate) mod tooltip;
mod wow;

use std::sync::Arc;

use app_core::Ports;
use axum::Extension;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use app_core::repo::Store;
use cluster_core::ClusterControl;

use crate::throttle::{AuthGate, LoginThrottle, SignUpThrottle, SseGate};
use crate::{assets, csrf, error, headers, metrics, prefs};

/// Build the application router.
///
/// Generic over the port bundle, so this function never names SQLite, the
/// Tokio-task cluster or Raider.IO.
pub fn router<E: Ports>(env: E, shutdown: crate::Shutdown) -> Router
where
    E::Hasher: Clone,
{
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

    // How the app is *running*: the cluster, its nodes, the jobs on it, the
    // request metrics, and what the tracker collects. Operations, not the
    // product -- somebody came for auction-house prices and has no use for a
    // node's heartbeat, and the deployment's health is not theirs to read.
    //
    // Gated by a layer rather than a check inside each handler: eleven
    // handlers each remembering to ask is eleven chances to forget, and the
    // one that forgets is the one that leaks.
    let operations = Router::new()
        .route("/", get(pages::dashboard::<E>))
        .route("/cluster", get(pages::cluster::<E>))
        .route("/nodes", get(pages::nodes::<E>))
        .route("/jobs", get(pages::jobs::<E>).post(jobs::submit::<E>))
        .route("/jobs/{id}", get(pages::job_detail::<E>))
        .route(
            "/admin",
            get(admin::page_handler::<E>).post(admin::toggle::<E>),
        )
        // Its own route rather than another `switch` value on the one above:
        // a switch is reversible and an activation archives the tier it
        // replaced, which is not the same kind of button.
        .route("/admin/release", post(admin::activate::<E>))
        // Writing down that something happened, and checking it afterwards.
        // Two routes because they are two decisions: an annotation lands
        // unvalidated whoever typed it, and publishing it is deliberate.
        .route("/admin/events", post(admin::add_event::<E>))
        .route("/admin/events/review", post(admin::review_event::<E>))
        .route("/nodes/{id}/roles", post(cluster::set_role::<E>))
        // The fragments behind those pages, which would otherwise answer the
        // same questions to anyone who asked them directly.
        .route("/partials/stats", get(partials::stats::<E>))
        .route("/partials/nodes", get(partials::nodes::<E>))
        .route("/partials/events", get(partials::events::<E>))
        .route("/partials/metrics", get(partials::metrics::<E>))
        .route("/partials/jobs", get(partials::jobs::<E>))
        .route("/partials/jobs/{id}", get(partials::job_detail::<E>))
        // JSON for scripts; the browser uses the fragments above.
        .route("/api/cluster", get(pages::snapshot_json::<E>))
        .route("/api/metrics", get(pages::metrics_json::<E>))
        .merge(debug_routes)
        .route_layer(axum::middleware::from_fn_with_state(
            env.clone(),
            crate::session::admin_only::<E>,
        ));

    Router::new()
        .merge(operations)
        // The product: what the auction house costs, and what the game's API
        // says about a character.
        .route("/account", get(pages::account::<E>))
        .route("/wow", get(pages::wow::<E>))
        .route("/wow/auctions", get(market::index::<E>))
        // Per account: what you follow, and what fired today. A visitor who is
        // signed out gets the page with an invitation and no alerts.
        .route(
            "/wow/alerts",
            get(alerts::page_handler::<E>).post(alerts::toggle::<E>),
        )
        .route("/wow/consumables", get(market::page_handler::<E>))
        .route("/wow/auctions/reagents", get(reagents::page_handler::<E>))
        .route(
            "/wow/auctions/enchants",
            get(enhancements::enchants_page::<E>),
        )
        .route("/wow/auctions/gems", get(enhancements::gems_page::<E>))
        .route("/wow/auctions/gear", get(gear::page_handler::<E>))
        .route("/wow/auctions/recipes", get(gear::recipes_page::<E>))
        // One page per upgrade track: the track is the market, and the item
        // levels inside it are what that page breaks apart.
        .route("/wow/gear/{item_id}/{track}", get(gear_stats::stats::<E>))
        // A recipe has one version of itself, so it has no item level.
        .route("/wow/recipe/{item_id}", get(gear_stats::recipe_stats::<E>))
        // The archive: expansion -> patch -> raid tier -> market analysis
        // (`docs/market-analysis.md` §8). Four levels, and each is validated
        // inside the one above it, so a real patch key from another expansion
        // is a 404 rather than a page about the wrong thing.
        .route("/wow/archive", get(archive::index::<E>))
        .route("/wow/archive/{expansion}", get(archive::expansion::<E>))
        .route("/wow/archive/{expansion}/{patch}", get(archive::patch::<E>))
        .route(
            "/wow/archive/{expansion}/{patch}/{tier}",
            get(archive::tier::<E>),
        )
        // Distinct prefixes so an expansion slug can never shadow an item id.
        .route("/wow/expansion/{id}", get(market::archived_page::<E>))
        .route("/wow/item/{item_id}", get(item::detail::<E>))
        // The cacheable half of the item page. A real URL rather than a
        // `/partials/` one because a reader with scripting off follows it as a
        // link, and it answers on its own.
        .route("/wow/item/{item_id}/analysis", get(item::analysis::<E>))
        .route("/wow/item/{item_id}/tooltip", get(tooltip::tooltip::<E>))
        // HTMX fragments
        .route("/partials/alerts", get(alerts::fragment::<E>))
        .route("/partials/realms", get(realms::fragment::<E>))
        .route("/partials/consumables", get(market::fragment::<E>))
        .route("/partials/patches", get(market::patches::<E>))
        .route("/partials/reagents", get(reagents::fragment::<E>))
        .route(
            "/partials/enchants",
            get(enhancements::enchants_fragment::<E>),
        )
        .route("/partials/gems", get(enhancements::gems_fragment::<E>))
        .route("/partials/gear", get(gear::fragment::<E>))
        .route("/partials/recipes", get(gear::recipes_fragment::<E>))
        .route("/wow/character", get(wow::character::<E>))
        // Live updates. The fragments above keep a slow poll as a fallback.
        .route("/events/stream", get(stream::events::<E>))
        // Liveness. No port call and therefore no database dependency: it
        // only proves the HTTP process can answer.
        .route("/healthz", get(healthz))
        // Readiness, which is a different question and needs its own answer.
        // A process can answer HTTP a long time before it has anything to
        // serve: the first start after a deployment materialises the archive,
        // and until it has, every page is a shell around nothing. A proxy that
        // sent traffic on `/healthz` alone would send it to those.
        //
        // It is also what stopped `scripts/bench.py` measuring a server whose
        // read model was still being built, which it did until this existed.
        .route("/readyz", get(readyz::<E>))
        // actions
        .route("/account/register", post(account::register::<E>))
        .route("/account/login", post(account::login::<E>))
        .route("/account/logout", post(account::logout::<E>))
        .route("/account/delete", post(account::delete::<E>))
        // assets
        .route("/static/pico.css", get(assets::pico))
        .route("/static/style.css", get(assets::style))
        .route("/static/htmx.min.js", get(assets::htmx))
        .route("/static/live.js", get(assets::live))
        .route("/favicon.ico", get(assets::favicon))
        .fallback(not_found)
        // How fast passwords may be guessed, and how fast accounts may be
        // asked about. One instance each for the process, because a limit
        // every request builds for itself is not a limit.
        // One bounded cache of rendered card fragments, shared by every
        // request. Like the throttles above: a cache each request builds for
        // itself is not a cache.
        .layer(Extension(Arc::new(crate::FragmentCache::new())))
        .layer(Extension(Arc::new(LoginThrottle::new())))
        .layer(Extension(Arc::new(SignUpThrottle::new())))
        .layer(Extension(Arc::new(AuthGate::default())))
        .layer(Extension(Arc::new(SseGate::default())))
        // Held by the handlers that keep a connection open, so they can let go
        // when the process is stopping rather than holding the door.
        .layer(Extension(shutdown))
        .layer(axum::middleware::from_fn_with_state(
            env.clone(),
            csrf::layer::<E>,
        ))
        // Inside `prefs`, so it knows the language; outside the handlers, so
        // it sees the error they returned. Renders that error's sentence in
        // the language the rest of the page is in.
        .layer(axum::middleware::from_fn(error::layer))
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
        // Outside compression, so the headers are on every response including
        // the ones the layers below never see the body of.
        .layer(axum::middleware::from_fn(headers::layer))
        .layer(TraceLayer::new_for_http())
        .with_state(env)
}

async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `GET /readyz` -- 204 once there is analysis to serve, 503 until then.
///
/// The published version's number goes in a header so an operator, or a
/// benchmark, can tell "ready" from "ready with the version I was expecting"
/// without a second request. Not gated behind the administrator layer: a probe
/// that needs a session is a probe a proxy cannot make, and "this instance has
/// published analysis" says no more than `/healthz` already does by answering.
async fn readyz<E: Ports>(State(env): State<E>) -> Response {
    use app_core::repo::ReadModelRepository;

    let cluster = match tokio::time::timeout(
        std::time::Duration::from_millis(250),
        env.cluster().snapshot(),
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(_) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "cluster is not responding").into_response();
        }
    };
    if cluster.persistence_queue >= 900 || cluster.persistence_oldest_ms >= 30_000 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable persistence is saturated",
        )
            .into_response();
    }

    match env.store().read_model().published().await {
        Ok(Some(version)) => (
            StatusCode::NO_CONTENT,
            [("x-analysis-version", version.version.to_string())],
        )
            .into_response(),
        Ok(None) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no published market analysis yet",
        )
            .into_response(),
        // A database that cannot be asked is not a ready one.
        Err(error) => {
            tracing::warn!(%error, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the read model could not be reached",
            )
                .into_response()
        }
    }
}

async fn not_found() -> Response {
    crate::error::WebError(app_core::AppError::NotFound).into_response()
}

#[cfg(test)]
mod tests {
    /// Every route module that serves the product, and the source of each.
    ///
    /// `include_str!` rather than reading the directory: the paths are checked
    /// at compile time, so a module that is renamed breaks the build instead
    /// of quietly dropping out of the check.
    const PUBLIC_ROUTES: &[(&str, &str)] = &[
        ("market.rs", include_str!("market.rs")),
        ("gear.rs", include_str!("gear.rs")),
        ("gear_stats.rs", include_str!("gear_stats.rs")),
        ("reagents.rs", include_str!("reagents.rs")),
        ("enhancements.rs", include_str!("enhancements.rs")),
        ("item.rs", include_str!("item.rs")),
        ("tooltip.rs", include_str!("tooltip.rs")),
        ("alerts.rs", include_str!("alerts.rs")),
        ("realms.rs", include_str!("realms.rs")),
        ("wow.rs", include_str!("wow.rs")),
        ("pages.rs", include_str!("pages.rs")),
        ("archive.rs", include_str!("archive.rs")),
    ];

    /// A `draft_ptr` catalogue is administrator-only
    /// (`docs/market-analysis.md` §8), and the gate is one function --
    /// `Ports::public_catalog` -- rather than a check every handler
    /// remembers. `CatalogSet::by_id` is the state-blind lookup underneath it,
    /// and a public route reaching past the gate to call it directly would
    /// serve a PTR catalogue to anybody who guessed its id.
    ///
    /// The same shape of rule as §7's operations gate, and the same reason for
    /// testing it structurally: eleven handlers each remembering to ask is
    /// eleven chances to forget, and the one that forgets is the one that
    /// leaks.
    #[test]
    fn no_public_route_reaches_past_the_catalogue_gate() {
        // Both state-blind lookups. `by_id` is the catalogue; `index` is the
        // same hole one level down, and it was open until Phase 9: the item
        // page resolved an id against every catalogue including a `draft_ptr`
        // one, so a guessed id answered with the next tier's candidate items.
        // `Ports::public_catalog` and `Ports::public_item` are the gates.
        const BLIND: [(&str, &str); 4] = [
            ("catalogs().by_id(", "Ports::public_catalog"),
            ("catalogs.by_id(", "Ports::public_catalog"),
            ("catalogs().index(", "Ports::public_item"),
            ("catalogs.index(", "Ports::public_item"),
        ];
        // Whitespace stripped before matching, because rustfmt breaks a long
        // chain across lines and `catalogs()\n.index()` is the same call. The
        // check missed `tooltip.rs` for exactly that reason -- a route that
        // was serving a PTR catalogue's item names, under a test that said it
        // could not.
        let mut offenders: Vec<String> = Vec::new();
        for (name, source) in PUBLIC_ROUTES {
            let dense: String = source.chars().filter(|c| !c.is_whitespace()).collect();
            for (needle, gate) in BLIND {
                if dense.contains(needle) {
                    offenders.push(format!("{name} calls {needle} instead of {gate}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these call a state-blind lookup, which serves a PTR catalogue -- or its \
             candidate item list -- to anybody who guesses an id: {offenders:#?}"
        );
    }

    /// And the check above is only worth having if the strings it looks for
    /// are ones that really appear when the rule is broken.
    #[test]
    fn the_gate_check_can_fail() {
        for broken in [
            "let c = env.catalogs().by_id(&id);",
            "env.catalogs().index().get(&item)",
            // The spelling rustfmt produces, which is the one that got past
            // the check when it matched the source as written.
            "env\n        .catalogs()\n        .index()\n        .get(&item)",
        ] {
            let dense: String = broken.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                dense.contains("catalogs().by_id(") || dense.contains("catalogs().index("),
                "{broken:?} should trip the check"
            );
        }
    }

    /// Every route Phase 2 moved onto the read model, which is every route
    /// that shows a price.
    const MATERIALISED_ROUTES: &[(&str, &str)] = &[
        ("market.rs", include_str!("market.rs")),
        ("reagents.rs", include_str!("reagents.rs")),
        ("enhancements.rs", include_str!("enhancements.rs")),
        ("item.rs", include_str!("item.rs")),
        ("alerts.rs", include_str!("alerts.rs")),
        ("gear.rs", include_str!("gear.rs")),
        ("gear_stats.rs", include_str!("gear_stats.rs")),
    ];

    /// CLAUDE.md §16, Phase 2: "No handler calls `analysis::analyse`, scans a
    /// full history, or calculates patch columns."
    ///
    /// That is the phase's exit condition rather than a style rule, and it is
    /// the kind that comes back: the reduction is easy to reach for, reads
    /// naturally at the call site, and costs nothing anybody notices until the
    /// archive is four months deep. So it is asserted rather than remembered.
    #[test]
    fn no_materialised_route_reduces_a_history() {
        let forbidden = [
            // The reduction itself.
            ("analyse(", "reduce a history during a request"),
            // Reading one, which is the only way to reduce one.
            (".history(", "read a full history"),
            (".history_in_region(", "read a whole region's history"),
            // The reduction the store used to do, once per patch column.
            (".window_stats(", "calculate a window during a request"),
            // The per-realm equivalents: rebuilding a region's current state
            // from the archive is what cost the Gear page ninety milliseconds.
            (
                ".latest_in_region(",
                "rebuild a region's markets from the archive",
            ),
            (
                ".window_in_region(",
                "read a whole region's per-realm window",
            ),
        ];
        let mut offenders: Vec<String> = Vec::new();
        for (name, source) in MATERIALISED_ROUTES {
            for (needle, what) in forbidden {
                if source.contains(needle) {
                    offenders.push(format!("{name} may still {what} ({needle})"));
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
    }
}
