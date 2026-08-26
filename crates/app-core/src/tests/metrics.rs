//! Metrics accounting. Cheap to test and easy to get subtly wrong.

use app_core::Metrics;

#[test]
fn requests_and_latency_are_averaged() {
    let metrics = Metrics::new();
    metrics.started();
    metrics.finished(200, 1_000);
    metrics.started();
    metrics.finished(200, 3_000);

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.requests_total, 2);
    assert_eq!(snapshot.mean_latency_micros, 2_000);
    assert_eq!(snapshot.mean_latency_ms(), 2.0);
    assert_eq!(snapshot.in_flight, 0);
}

#[test]
fn errors_are_split_by_class() {
    let metrics = Metrics::new();
    for status in [200, 204, 404, 403, 500] {
        metrics.started();
        metrics.finished(status, 10);
    }
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.requests_total, 5);
    assert_eq!(snapshot.client_errors, 2);
    assert_eq!(snapshot.server_errors, 1);
}

#[test]
fn concurrency_tracks_a_high_water_mark() {
    let metrics = Metrics::new();
    metrics.started();
    metrics.started();
    metrics.started();
    assert_eq!(metrics.snapshot().in_flight, 3);

    metrics.finished(200, 10);
    metrics.finished(200, 10);
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.in_flight, 1);
    assert_eq!(snapshot.peak_in_flight, 3, "the peak is not forgotten");
}

#[test]
fn a_closed_stream_frees_its_slot_without_being_counted() {
    // An SSE connection held open for minutes must not be reported as a
    // minutes-long request latency.
    let metrics = Metrics::new();
    metrics.started();
    metrics.finished(200, 1_000);

    metrics.started();
    metrics.connection_closed();

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.requests_total, 1, "the stream is not a request");
    assert_eq!(snapshot.mean_latency_micros, 1_000, "and not a sample");
    assert_eq!(snapshot.in_flight, 0, "but it did release its slot");
    assert_eq!(snapshot.peak_in_flight, 1);
}

#[test]
fn an_empty_snapshot_does_not_divide_by_zero() {
    assert_eq!(Metrics::new().snapshot().mean_latency_micros, 0);
}
