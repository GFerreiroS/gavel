use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct PersistenceQueue {
    depth: usize,
    oldest: Option<std::time::Instant>,
}

#[derive(Debug, Default)]
pub(crate) struct Telemetry {
    connections: AtomicUsize,
    preauth: AtomicUsize,
    rejected: AtomicU64,
    persistence_queue: Mutex<PersistenceQueue>,
    persistence_errors: AtomicU64,
    jobs_recovered: AtomicU64,
    task_retries: AtomicU64,
}

impl Telemetry {
    pub fn connection_opened(&self) {
        self.connections.fetch_add(1, Ordering::Relaxed);
    }
    pub fn connection_closed(&self) {
        self.connections.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn preauth_opened(&self) {
        self.preauth.fetch_add(1, Ordering::Relaxed);
    }
    pub fn preauth_closed(&self) {
        self.preauth.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn rejected(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn queued(&self) {
        let mut queue = self
            .persistence_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if queue.depth == 0 {
            queue.oldest = Some(std::time::Instant::now());
        }
        queue.depth = queue.depth.saturating_add(1);
    }
    pub fn dequeued(&self) {
        let mut queue = self
            .persistence_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        queue.depth = queue.depth.saturating_sub(1);
        if queue.depth == 0 {
            queue.oldest = None;
        }
    }
    pub fn persistence_error(&self) {
        self.persistence_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn recovered(&self, count: usize) {
        self.jobs_recovered
            .fetch_add(count as u64, Ordering::Relaxed);
    }
    pub fn retried(&self) {
        self.task_retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn apply(&self, snapshot: &mut cluster_core::ClusterSnapshot) {
        snapshot.worker_connections = self.connections.load(Ordering::Relaxed);
        snapshot.worker_preauth = self.preauth.load(Ordering::Relaxed);
        snapshot.worker_rejected = self.rejected.load(Ordering::Relaxed);
        let queue = self
            .persistence_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        snapshot.persistence_queue = queue.depth;
        snapshot.persistence_oldest_ms = queue
            .oldest
            .map(|instant| instant.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        snapshot.persistence_errors = self.persistence_errors.load(Ordering::Relaxed);
        snapshot.jobs_recovered = self.jobs_recovered.load(Ordering::Relaxed);
        snapshot.task_retries = self.task_retries.load(Ordering::Relaxed);
    }
}

pub(crate) struct ConnectionGuard {
    telemetry: std::sync::Arc<Telemetry>,
    preauth: bool,
}

impl ConnectionGuard {
    pub fn new(telemetry: std::sync::Arc<Telemetry>) -> Self {
        telemetry.connection_opened();
        telemetry.preauth_opened();
        Self {
            telemetry,
            preauth: true,
        }
    }
    pub fn authenticated(&mut self) {
        if self.preauth {
            self.telemetry.preauth_closed();
            self.preauth = false;
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if self.preauth {
            self.telemetry.preauth_closed();
        }
        self.telemetry.connection_closed();
    }
}
