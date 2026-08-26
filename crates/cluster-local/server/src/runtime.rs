//! The concrete port bundle.
//!
//! This is the one place that names SQLite, the Tokio-task cluster and
//! Raider.IO together. `app-web` sees only `Ports`.

use std::sync::Arc;

use app_core::auth::{Argon2Hasher, OsTokens};
use app_core::{Ports, WebConfig};
use app_integrations::RaiderIoClient;
use cluster_local::{LocalCluster, SystemClock};
use storage::SqliteStore;

pub struct Inner {
    pub store: SqliteStore,
    pub cluster: LocalCluster,
    pub characters: RaiderIoClient<SystemClock>,
    pub hasher: Argon2Hasher,
    pub tokens: OsTokens,
    pub clock: SystemClock,
    pub config: WebConfig,
}

/// Cheap to clone: Axum clones the state for every request.
pub struct Runtime(Arc<Inner>);

impl Runtime {
    pub fn new(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

impl Clone for Runtime {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl Ports for Runtime {
    type Store = SqliteStore;
    type Cluster = LocalCluster;
    type Characters = RaiderIoClient<SystemClock>;
    type Hasher = Argon2Hasher;
    type Tokens = OsTokens;
    type Clock = SystemClock;

    fn store(&self) -> &Self::Store {
        &self.0.store
    }
    fn cluster(&self) -> &Self::Cluster {
        &self.0.cluster
    }
    fn characters(&self) -> &Self::Characters {
        &self.0.characters
    }
    fn hasher(&self) -> &Self::Hasher {
        &self.0.hasher
    }
    fn tokens(&self) -> &Self::Tokens {
        &self.0.tokens
    }
    fn clock(&self) -> &Self::Clock {
        &self.0.clock
    }
    fn config(&self) -> &WebConfig {
        &self.0.config
    }
}
