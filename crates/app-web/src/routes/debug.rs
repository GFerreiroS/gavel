//! Failure-simulation controls.
//!
//! Mounted only when `WebConfig::debug_controls` is set, which the server ties
//! to an explicit flag. These are development affordances, not product
//! features, and they must never appear on a normal deployment.
//!
//! Parameters go in the query string rather than a form body so that the same
//! endpoints are usable from curl and from a script, not only from the browser.
//! CSRF therefore comes from the `X-CSRF-Token` header, which HTMX sends via
//! `hx-headers` on `<body>`.

use app_core::Ports;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Html;
use cluster_core::{ClusterControl, NodeId};
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::routes::partials;

#[derive(Debug, Deserialize, Default)]
pub struct DebugParams {
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub millis: u64,
    /// How many consecutive tasks to fail. Defaults to one.
    #[serde(default)]
    pub count: Option<u32>,
}

/// Every control does the same three things: check CSRF, poke the runtime,
/// re-render the node list.
macro_rules! debug_handler {
    ($name:ident, $log:literal, |$env:ident, $node:ident, $params:ident| $body:expr) => {
        pub async fn $name<E: Ports>(
            State($env): State<E>,
            Extension(csrf): Extension<Csrf>,
            Extension(prefs): Extension<MarketPrefs>,
            headers: HeaderMap,
            Path(id): Path<u16>,
            Query($params): Query<DebugParams>,
        ) -> WebResult<Html<String>> {
            csrf.verify_request(&headers, None)?;
            let $node = NodeId(id);
            $body;
            tracing::warn!(node = %$node, "debug control: {}", $log);
            page(&partials::nodes_fragment(&$env, prefs.locale).await, prefs.locale)
        }
    };
}

debug_handler!(stop_node, "node stopped", |env, node, _params| {
    env.cluster().stop_node(node).await?
});

debug_handler!(start_node, "node started", |env, node, _params| {
    env.cluster().start_node(node).await?
});

debug_handler!(
    toggle_heartbeat,
    "heartbeat toggled",
    |env, node, params| { env.cluster().pause_heartbeat(node, params.paused).await? }
);

debug_handler!(inject_failure, "failure injected", |env, node, params| {
    env.cluster()
        .inject_failures(node, params.count.unwrap_or(1))
        .await?
});

debug_handler!(set_delay, "task delay set", |env, node, params| {
    env.cluster().set_task_delay(node, params.millis).await?
});
