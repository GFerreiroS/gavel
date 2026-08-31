//! Server-sent events.
//!
//! The browser opens one connection and gets told when something actually
//! happened, instead of asking twice a second. Polling stays configured as a
//! slow fallback so the page still updates if the stream drops.
//!
//! Only the event *kind* goes over the wire: the page then re-fetches the
//! fragments it cares about. That keeps one live connection per browser
//! inexpensive even when the cluster is busy.
//!
//! **Two audiences on one stream.** Everybody needs the nudge -- a price page
//! refreshes when a collection job finishes, exactly like a node list -- but
//! `task_failed` and `leader_lost` describe how the deployment is doing, and
//! that is operations, not product (CLAUDE.md §7). So a signed-in
//! administrator gets the real kind and everybody else gets the fact that
//! *something* changed, which is all `live.js` has ever read.

use std::convert::Infallible;
use std::time::Duration;

use app_core::Ports;
use axum::Extension;
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use cluster_core::ClusterControl;
use tokio_stream::{Stream, StreamExt};

use crate::session::current_user;
use crate::shutdown::Shutdown;
use crate::throttle::SseGate;

struct MetricsGuard<E: Ports>(E);

impl<E: Ports> Drop for MetricsGuard<E> {
    fn drop(&mut self) {
        self.0.metrics().sse_closed();
    }
}

/// What a visitor is told instead of the event's name.
pub const PUBLIC_KIND: &str = "changed";

pub async fn events<E: Ports>(
    State(env): State<E>,
    Extension(shutdown): Extension<Shutdown>,
    Extension(gate): Extension<std::sync::Arc<SseGate>>,
    connect: ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let origin = if env.config().trust_proxy_headers {
        headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .and_then(|value| value.trim().parse().ok())
    } else {
        None
    }
    .unwrap_or_else(|| connect.0.ip());
    let permit = match gate.enter(origin) {
        Some(permit) => permit,
        None => {
            env.metrics().sse_rejected();
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    };
    env.metrics().sse_opened();
    let metrics_guard = MetricsGuard(env.clone());
    // Resolved once, when the connection opens. A session that expires while
    // the tab is open keeps its detail until the stream reconnects, which is
    // the same grace any long-lived connection gets and costs nothing: this
    // stream carries no data, only names.
    let detailed = current_user(&env, &headers)
        .await
        .ok()
        .flatten()
        .is_some_and(|user| user.is_admin);

    let stream = env.cluster().subscribe().map(move |record| {
        let _keep_connection_permit = &permit;
        let _keep_metrics_guard = &metrics_guard;
        let kind = if detailed {
            record.event.kind()
        } else {
            PUBLIC_KIND
        };
        Ok(Event::default().event("cluster").data(kind))
    });

    // Ends when the server does. Graceful shutdown waits for the responses
    // already in flight, and this one never finishes on its own -- so one
    // open browser tab used to make Ctrl+C hang for ever. A stream is not
    // really "in flight"; it is a connection waiting for something to happen,
    // and when nothing more will happen it should let go.
    let stream = crate::shutdown::until(stream, shutdown);

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}
