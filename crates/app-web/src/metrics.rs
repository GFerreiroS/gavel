//! Request-timing middleware.
//!
//! Feeds `app_core::Metrics`, which is what a future autoscaler reads to decide
//! whether the frontend/backend roles need more replicas, and -- when asked
//! for -- `app_core::timing`, which is what a change to the read path has to
//! point at to claim it made anything faster.

use std::time::Instant;

use app_core::Ports;
use app_core::timing::{self, Timings};
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use http_body::Body as _;

/// Long-lived streams are connections, not requests.
const STREAM_PATH: &str = "/events/stream";

const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

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

    // Nothing accumulates unless this is on: with no ambient accounting
    // installed, every `timing::start` below costs one thread-local read and
    // returns `None`.
    let response = if env.config().server_timing {
        let timings = Timings::new();
        // The statement counters are the process's, because the SQLite driver
        // reports from its own threads and cannot be asked whose request it
        // was serving. Sequentially -- how the benchmark asks -- this
        // difference is exactly this request's; under concurrent traffic it is
        // the process's database work while this request ran.
        let before = timing::DATABASE.read();
        let mut response = timing::scope(timings.clone(), next.run(request)).await;
        timings.absorb(timing::DATABASE.read().since(before));
        // Measured here rather than outside the compression layer, so this is
        // the uncompressed body -- the bytes the browser has to parse, style
        // and lay out, which is the number §11b found mattered as much as the
        // wire size.
        let bytes = response.body().size_hint().exact();
        stamp(&mut response, &timings, started, bytes);
        response
    } else {
        next.run(request).await
    };

    metrics.finished(
        response.status().as_u16(),
        started.elapsed().as_micros() as u64,
    );
    response
}

fn stamp(response: &mut Response, timings: &Timings, started: Instant, bytes: Option<u64>) {
    let mut value = timings.header(started.elapsed().as_micros() as u64);
    if let Some(bytes) = bytes {
        value.push_str(&format!(", bytes;desc=\"{bytes}\""));
    }
    // The value is built here from integers and fixed keys, so it cannot fail
    // to parse; if it somehow did, a missing diagnostic header is not a reason
    // to fail the request.
    if let Ok(header) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(SERVER_TIMING, header);
    }
}
