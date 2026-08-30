//! Auction-house market tracking.
//!
//! Scope note: everything tracked here is a **commodity**, which in retail WoW
//! means the market is region-wide. There is no per-realm price for a flask or
//! a potion -- the whole EU region sees one set of listings. Non-commodity
//! items (gear, BoEs, mounts) are per connected realm and will need a second,
//! realm-dimensioned path when we get to them.

pub mod alerts;
pub mod analysis;
pub mod catalog;
pub mod collector;
pub mod key;
pub mod realm;
pub mod release;
pub mod stats;

use std::fmt;

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

pub use alerts::{Alert, AlertRule, AlertSeverity};
pub use analysis::{Cycle, ItemAnalysis, Point, Trend, analyse, downsample};
pub use catalog::{
    ALL_AUDIENCES, ALL_AUDIENCES_LABELS, ALL_PROFESSIONS, ALL_SLOTS, Audience, Catalog,
    CatalogItem, CatalogSet, CatalogStatus, Category, ItemKind, ItemLevel, ItemRank, Patch,
    Profession, Slot, Stat, Track,
};
pub use collector::{AlertSink, Collector, NullSink, Outcome, Report};
pub use key::{BadMarketKey, MarketKey};
pub use realm::{
    GearListing, Realm, RealmAuctionProvider, RealmId, RealmSample, RealmSnapshot, summarise_realm,
};
pub use release::ReleaseStates;

pub use stats::{PriceStats, summarise};

/// A WoW item id. Distinct from our own ids so the two cannot be confused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub u32);

impl ItemId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A Battle.net region. Commodity markets are per-region and completely
/// separate: an EU price tells you nothing about US.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    Eu,
    Us,
    Kr,
    Tw,
}

pub const ALL_REGIONS: [Region; 4] = [Region::Eu, Region::Us, Region::Kr, Region::Tw];

impl Region {
    pub const fn as_str(self) -> &'static str {
        match self {
            Region::Eu => "eu",
            Region::Us => "us",
            Region::Kr => "kr",
            Region::Tw => "tw",
        }
    }

    /// Host for the game-data API. Each region is a separate deployment.
    pub const fn api_host(self) -> &'static str {
        match self {
            Region::Eu => "https://eu.api.blizzard.com",
            Region::Us => "https://us.api.blizzard.com",
            Region::Kr => "https://kr.api.blizzard.com",
            Region::Tw => "https://tw.api.blizzard.com",
        }
    }

    /// Dynamic namespace, required on every game-data request.
    pub fn namespace(self) -> String {
        format!("dynamic-{}", self.as_str())
    }

    /// Static namespace, for data that only changes when the game patches:
    /// item names, qualities, effects. Separate from [`Region::namespace`]
    /// because asking the wrong namespace is a 404, not a fallback.
    pub fn static_namespace(self) -> String {
        format!("static-{}", self.as_str())
    }

    pub fn parse(s: &str) -> Option<Region> {
        ALL_REGIONS
            .into_iter()
            .find(|r| r.as_str().eq_ignore_ascii_case(s))
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Prices are integer copper throughout -- no floating point anywhere in the
/// money path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Copper(pub u64);

impl Copper {
    pub const ZERO: Copper = Copper(0);

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn gold(self) -> u64 {
        self.0 / 10_000
    }

    pub const fn silver(self) -> u64 {
        (self.0 % 10_000) / 100
    }

    pub const fn copper(self) -> u64 {
        self.0 % 100
    }
}

impl fmt::Display for Copper {
    /// `1234g 56s` -- copper is dropped above a gold because nobody reads it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 >= 10_000 {
            write!(f, "{}g {:02}s", self.gold(), self.silver())
        } else if self.0 >= 100 {
            write!(f, "{}s {:02}c", self.silver(), self.copper())
        } else {
            write!(f, "{}c", self.copper())
        }
    }
}

/// One auction listing, reduced to what a price tracker needs.
///
/// The API also returns bid, auction id and time_left; none of them matter for
/// commodities, which are unit-priced and bought instantly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listing {
    pub item: ItemId,
    pub unit_price: Copper,
    pub quantity: u64,
}

/// A single observation of one item's market at one point in time.
///
/// This is what gets persisted every hour and what the history is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceSample {
    pub item: ItemId,
    pub region: Region,
    pub observed_at: Millis,
    /// Cheapest unit price on the market -- the price you would actually pay.
    pub min_unit_price: Copper,
    /// Price to buy out the cheapest 5% of supply; resistant to a single
    /// troll listing at 1 copper.
    pub p05_unit_price: Copper,
    pub median_unit_price: Copper,
    /// Total units listed. A cheap price on 3 units is not a buying signal.
    pub quantity: u64,
    /// Number of distinct auctions, as a liquidity hint.
    pub listings: u32,
}

/// What a commodity snapshot fetch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// The upstream copy has not changed since the timestamp we sent.
    ///
    /// Worth honouring: the commodities endpoint costs 25 against the hourly
    /// request budget instead of 1, and the data only moves once an hour.
    NotModified,
    Fresh {
        /// `Last-Modified` from the response -- the moment Blizzard generated
        /// the snapshot, which is the honest observation time. Using our own
        /// clock would smear samples across the hour boundary.
        generated_at: Millis,
        listings: Vec<Listing>,
    },
}

/// Reads the region-wide commodity auction house.
pub trait CommodityProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &'static str;

    /// Whether this provider can actually reach upstream. A provider without
    /// credentials is still constructible -- the app must start without them
    /// -- so the UI needs to be able to say so rather than showing an
    /// unexplained empty table.
    fn is_configured(&self) -> bool {
        true
    }

    /// Fetch the snapshot for `region`, keeping only listings whose item is in
    /// `wanted`.
    ///
    /// Filtering is the provider's job because the raw payload is far larger
    /// than the part we care about. Discarding unrelated listings while
    /// parsing also avoids a large short-lived allocation every cycle.
    fn commodities(
        &self,
        region: Region,
        wanted: &[ItemId],
        if_modified_since: Option<Millis>,
    ) -> impl std::future::Future<Output = crate::AppResult<Snapshot>> + Send;
}

/// How the tracker is configured to run.
#[derive(Debug, Clone)]
pub struct MarketConfig {
    /// Regions to collect. Commodity markets are completely separate, so each
    /// one is its own collection and its own history.
    pub regions: Vec<Region>,
    /// Connected realms to collect gear prices from, as (region, realm).
    ///
    /// A short list on purpose: one realm is ~20 MB per cycle, and every
    /// realm in EU and US would be roughly half a gigabyte. The eventual
    /// answer is to fan this out across the cluster -- one task per realm is
    /// exactly the shape the scheduler already takes -- but a handful of
    /// realms is honestly a loop.
    pub realms: Vec<(Region, RealmId)>,
    pub rule: AlertRule,
    /// How often to poll. The upstream snapshot only moves hourly, so polling
    /// faster mostly produces 304s -- cheap, but not free at 25 per call.
    pub collect_interval_ms: u64,
    /// How long history is kept before pruning. **Zero means keep forever**,
    /// which is the default: the point of this feature is the archive, and a
    /// retention window would quietly delete the oldest expansion first.
    ///
    /// Volume is handled by [`Self::downsample_after_ms`] instead, which keeps
    /// the archive and drops only its resolution.
    pub retain_ms: u64,
    /// How long samples stay at full resolution before each day of them is
    /// collapsed into a single row. Zero disables it.
    ///
    /// This is the answer to growth, rather than pruning. The catalogue is no
    /// longer the twenty-six items the retention note above was written for:
    /// six hundred commodity ids across four regions, plus gear and recipes on
    /// every connected realm, is millions of rows a year at hourly
    /// resolution. A day-old price is worth knowing; the fact that it was
    /// collected at 14:00 rather than 15:00 is not.
    pub downsample_after_ms: u64,
}

impl Default for MarketConfig {
    fn default() -> Self {
        Self {
            regions: vec![Region::Eu],
            realms: Vec::new(),
            rule: AlertRule::default(),
            collect_interval_ms: 30 * 60 * 1000,
            retain_ms: 0,
            downsample_after_ms: 14 * 24 * 60 * 60 * 1000,
        }
    }
}

/// Aggregate view of one item over a window, for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowStats {
    pub item: ItemId,
    pub low: Copper,
    /// When the low was observed. "Cheapest ever" is only actionable if you
    /// know whether that was yesterday or four months ago.
    pub low_at: Millis,
    pub high: Copper,
    pub high_at: Millis,
    pub mean: Copper,
    pub samples: u32,
}
