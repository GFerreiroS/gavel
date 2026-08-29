//! Server-sent events.
//!
//! The browser opens one connection and gets told when something actually
//! happened, instead of asking twice a second. Polling stays configured as a
//! slow fallback so the page still updates if the stream drops.
//!
//! Only the event *kind* goes over the wire: the page then re-fetches the
//! fragments it cares about. That keeps one live connection per browser
//! inexpensive even when the cluster is busy.

use std::convert::Infallible;
use std::time::Duration;

use app_core::Ports;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use cluster_core::ClusterControl;
use tokio_stream::{Stream, StreamExt};

pub async fn events<E: Ports>(
    State(env): State<E>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = env
        .cluster()
        .subscribe()
        .map(|record| Ok(Event::default().event("cluster").data(record.event.kind())));

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
