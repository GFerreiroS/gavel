//! The single bundle of dependencies the web layer is generic over.
//!
//! `app-web` takes one type parameter `E: Ports` instead of six, and stays
//! free of `Box<dyn ...>` and per-request allocation.

use cluster_core::{Clock, ClusterControl, Millis};

use crate::auth::{PasswordHasher, TokenSource};
use crate::item::ItemDetailProvider;
use crate::market::{
    AlertSink, Catalog, CatalogSet, CommodityProvider, MarketConfig, RealmAuctionProvider,
};
use crate::metrics::Metrics;
use crate::repo::Store;
use crate::wow::CharacterProvider;

/// Presentation-affecting settings, resolved once at startup.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub app_name: String,
    /// How often HTMX partials re-poll, in milliseconds. Becomes the SSE
    /// reconnect hint when the transport is switched later.
    pub poll_interval_ms: u64,
    /// Number of events shown in the UI event log.
    pub event_log_limit: usize,
    /// Mount the failure-simulation routes. Off unless explicitly asked for.
    pub debug_controls: bool,
    /// Send `Secure` on the session cookie. Off for plain-HTTP local dev.
    pub secure_cookies: bool,
    /// How long upstream WoW responses stay cached.
    pub upstream_cache_ttl_ms: u64,
    /// How long static item data (names, effects, qualities) stays cached.
    /// Far longer than `upstream_cache_ttl_ms`, because this data only moves
    /// when the game patches.
    pub item_cache_ttl_ms: u64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            app_name: "Auction Tracker".to_string(),
            poll_interval_ms: 2_000,
            event_log_limit: 25,
            debug_controls: false,
            secure_cookies: false,
            upstream_cache_ttl_ms: 10 * 60 * 1000,
            item_cache_ttl_ms: 7 * 24 * 60 * 60 * 1000,
        }
    }
}

pub trait Ports: Clone + Send + Sync + 'static {
    type Store: Store;
    type Cluster: ClusterControl;
    type Characters: CharacterProvider;
    type Commodities: CommodityProvider;
    type RealmAuctions: RealmAuctionProvider;
    type Items: ItemDetailProvider;
    type Alerts: AlertSink;
    type Hasher: PasswordHasher;
    type Tokens: TokenSource;
    type Clock: Clock;

    fn store(&self) -> &Self::Store;
    fn cluster(&self) -> &Self::Cluster;
    fn characters(&self) -> &Self::Characters;
    fn commodities(&self) -> &Self::Commodities;
    /// The per-realm auction houses, where gear is sold.
    fn realm_auctions(&self) -> &Self::RealmAuctions;
    /// Static item data, for tooltips.
    fn items(&self) -> &Self::Items;
    fn alert_sink(&self) -> &Self::Alerts;
    /// Every expansion's catalog: one active, the rest archived.
    fn catalogs(&self) -> &CatalogSet;
    fn market(&self) -> &MarketConfig;
    fn hasher(&self) -> &Self::Hasher;
    fn tokens(&self) -> &Self::Tokens;
    fn clock(&self) -> &Self::Clock;
    fn config(&self) -> &WebConfig;

    /// Request-side counters, fed by the metrics middleware.
    fn metrics(&self) -> &Metrics;

    fn now(&self) -> Millis {
        self.clock().now()
    }

    /// The expansion currently being collected, if any.
    fn active_catalog(&self) -> Option<&Catalog> {
        self.catalogs().active()
    }
}
