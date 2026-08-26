//! The counters a future autoscaler will need (CLAUDE.md 23).
//!
//! Plain atomics rather than a metrics framework: the whole point is that this
//! has to be affordable on a node with a few hundred KB of RAM. Queue depth,
//! worker load and running jobs come from the cluster snapshot; what the
//! cluster cannot see is the HTTP side, which is what this collects.

use std::sync::atomic::{AtomicU64, Ordering};

/// Fixed-cost request counters. Cloneable by sharing, not by copying.
#[derive(Debug, Default)]
pub struct Metrics {
    requests_total: AtomicU64,
    responses_client_error: AtomicU64,
    responses_server_error: AtomicU64,
    latency_micros_total: AtomicU64,
    in_flight: AtomicU64,
    peak_in_flight: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// A request has arrived. Returns nothing; pair with [`Metrics::finished`].
    pub fn started(&self) {
        let now = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        // Monotonic high-water mark, retried only while we are behind.
        let mut peak = self.peak_in_flight.load(Ordering::Relaxed);
        while now > peak {
            match self.peak_in_flight.compare_exchange_weak(
                peak,
                now,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => peak = observed,
            }
        }
    }

    /// A long-lived connection ended. Releases the concurrency slot without
    /// recording a request or a latency sample.
    pub fn connection_closed(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn finished(&self, status: u16, latency_micros: u64) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.latency_micros_total
            .fetch_add(latency_micros, Ordering::Relaxed);
        match status {
            400..=499 => {
                self.responses_client_error.fetch_add(1, Ordering::Relaxed);
            }
            500..=599 => {
                self.responses_server_error.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let requests = self.requests_total.load(Ordering::Relaxed);
        let micros = self.latency_micros_total.load(Ordering::Relaxed);
        MetricsSnapshot {
            requests_total: requests,
            client_errors: self.responses_client_error.load(Ordering::Relaxed),
            server_errors: self.responses_server_error.load(Ordering::Relaxed),
            mean_latency_micros: micros.checked_div(requests).unwrap_or(0),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            peak_in_flight: self.peak_in_flight.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsSnapshot {
    pub requests_total: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub mean_latency_micros: u64,
    /// Requests being served right now -- the "active connections" signal.
    pub in_flight: u64,
    pub peak_in_flight: u64,
}

impl MetricsSnapshot {
    pub fn mean_latency_ms(&self) -> f64 {
        self.mean_latency_micros as f64 / 1000.0
    }
}
