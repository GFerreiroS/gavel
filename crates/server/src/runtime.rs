//! The concrete port bundle.
//!
//! This is the one place that names SQLite, the Tokio-task cluster and
//! Raider.IO together. `app-web` sees only `Ports`.

use std::sync::Arc;

use app_core::auth::{Argon2Hasher, OsTokens};
use app_core::market::{CatalogSet, MarketConfig, ReleaseStates};
use app_core::{Metrics, Ports, WebConfig};
use app_integrations::RaiderIoClient;
use cluster_local::{LocalCluster, SystemClock};
use storage::SqliteStore;

use crate::market::{Alerts, Commodities, Items, RealmAuctions};

pub struct Inner {
    pub store: SqliteStore,
    pub cluster: LocalCluster,
    pub characters: RaiderIoClient<SystemClock>,
    pub commodities: Commodities,
    pub realm_auctions: RealmAuctions,
    pub items: Items,
    pub alerts: Alerts,
    pub catalogs: CatalogSet,
    /// Where each catalogue is in its life. Loaded at startup and replaced
    /// when an administrator activates one, so a page reads a map rather than
    /// the database.
    pub releases: ReleaseStates,
    pub market: MarketConfig,
    pub hasher: Arc<Argon2Hasher>,
    pub tokens: OsTokens,
    pub clock: SystemClock,
    pub config: WebConfig,
    pub metrics: Arc<Metrics>,
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
    type Commodities = Commodities;
    type RealmAuctions = RealmAuctions;
    type Items = Items;
    type Alerts = Alerts;
    type Hasher = Arc<Argon2Hasher>;
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
    fn commodities(&self) -> &Self::Commodities {
        &self.0.commodities
    }
    fn realm_auctions(&self) -> &Self::RealmAuctions {
        &self.0.realm_auctions
    }
    fn items(&self) -> &Self::Items {
        &self.0.items
    }
    fn alert_sink(&self) -> &Self::Alerts {
        &self.0.alerts
    }
    fn catalogs(&self) -> &CatalogSet {
        &self.0.catalogs
    }

    fn releases(&self) -> &ReleaseStates {
        &self.0.releases
    }
    fn market(&self) -> &MarketConfig {
        &self.0.market
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
    fn metrics(&self) -> &Metrics {
        &self.0.metrics
    }
}
