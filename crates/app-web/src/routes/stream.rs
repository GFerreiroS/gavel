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
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use cluster_core::ClusterControl;
use tokio_stream::{Stream, StreamExt};

use crate::session::current_user;
use crate::shutdown::Shutdown;

/// What a visitor is told instead of the event's name.
pub const PUBLIC_KIND: &str = "changed";

pub async fn events<E: Ports>(
    State(env): State<E>,
    Extension(shutdown): Extension<Shutdown>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
