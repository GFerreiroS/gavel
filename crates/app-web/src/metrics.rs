//! Request-timing middleware.
//!
//! Feeds `app_core::Metrics`, which is what a future autoscaler reads to decide
//! whether the frontend/backend roles need more replicas (CLAUDE.md 23).

use std::time::Instant;

use app_core::Ports;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

/// Long-lived streams are connections, not requests.
const STREAM_PATH: &str = "/events/stream";

pub async fn layer<E: Ports>(State(env): State<E>, request: Request, next: Next) -> Response {
    let path = request.uri().path();

    // Asset requests would drown out everything interesting.
    if path.starts_with("/static/") || path == "/favicon.ico" {
        return next.run(request).await;
    }

    let metrics = env.metrics();

    // An SSE connection is held open for as long as the tab is; counting it as
    // a request would report its lifetime as latency and skew the mean beyond
    // usefulness. It still counts towards concurrency, which is the number an
    // autoscaler actually wants from it.
    if path == STREAM_PATH {
        metrics.started();
        let response = next.run(request).await;
        metrics.connection_closed();
        return response;
    }

    metrics.started();
    let started = Instant::now();
    let response = next.run(request).await;
    metrics.finished(
        response.status().as_u16(),
        started.elapsed().as_micros() as u64,
    );
    response
}
