//! The single bundle of dependencies the web layer is generic over.
//!
//! `app-web` takes one type parameter `E: Ports` instead of six, and stays
//! free of `Box<dyn ...>` and per-request allocation.

use cluster_core::{Clock, ClusterControl, Millis};

use crate::auth::{PasswordHasher, TokenSource};
use crate::item::ItemDetailProvider;
use crate::market::catalog::CatalogStatus;
use crate::market::{
    AlertSink, Catalog, CatalogSet, CommodityProvider, MarketConfig, RealmAuctionProvider,
    ReleaseStates, release,
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
    /// Break each response's time down in a `Server-Timing` header.
    ///
    /// **Off by default**, like the failure-simulation routes and for the same
    /// reason: per-stage timings, statement counts and row counts say how the
    /// deployment is doing, which §7 keeps on the operations side of the app.
    /// A visitor is owed the page, not the shape of the read path behind it.
    /// The benchmark turns it on; a deployment asks for it when it wants it.
    pub server_timing: bool,
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
            server_timing: false,
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
    /// Every catalogue this build ships, in whatever state.
    ///
    /// The *content*. Where each one is in its life is [`Self::releases`], and
    /// the two are separate because content is reviewed code and state is a
    /// runtime decision (`market::release`).
    fn catalogs(&self) -> &CatalogSet;

    /// Where each catalogue is in its life, as the database last said.
    fn releases(&self) -> &ReleaseStates;
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

    /// Where one catalogue is in its life.
    ///
    /// The deployment's answer, not the file's. `Catalog::shipped_status` is
    /// the file's, and it is only ever the seed.
    fn catalog_state(&self, catalog: &Catalog) -> CatalogStatus {
        release::state_of(self.releases(), catalog)
    }

    /// The expansion currently being collected, if any.
    ///
    /// `None` is a legal answer and not a broken instance: an expansion that
    /// has ended while its successor is still a `draft_ptr` has nothing
    /// active, and the pages say "archived" rather than falling over.
    fn active_catalog(&self) -> Option<&Catalog> {
        release::active(self.catalogs(), self.releases())
    }

    /// A catalogue by id, for anybody.
    ///
    /// `None` for a `draft_ptr` one, which is what keeps §8's
    /// "administrator-only, and lists no prices" true without eleven handlers
    /// each remembering to ask -- the same reasoning §7 applies to the
    /// operations pages, and for the same reason: the handler that forgets is
    /// the one that leaks.
    fn public_catalog(&self, id: &str) -> Option<&Catalog> {
        release::public(self.catalogs(), self.releases(), id)
    }

    /// Every catalogue a visitor may see, in display order.
    fn public_catalogs(&self) -> Vec<&Catalog> {
        release::public_all(self.catalogs(), self.releases())
    }

    /// Every catalogue, in display order. For `/admin`, which is the one place
    /// a `draft_ptr` catalogue is visible.
    fn all_catalogs(&self) -> Vec<&Catalog> {
        release::all(self.catalogs(), self.releases())
    }
}
