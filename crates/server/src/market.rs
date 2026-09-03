//! Wiring for the auction-house tracker.
//!
//! Both the data source and the notification channel are optional: the app has
//! to start and be useful without Battle.net credentials or a Discord webhook.
//! Rather than making them `Option` everywhere, each has an "unconfigured"
//! variant that reports the problem at the point of use.

use app_core::error::{AppError, AppResult};
use app_core::item::{ItemDetailProvider, LocalizedTooltips};
use app_core::market::{
    Alert, AlertSink, CommodityProvider, ItemId, MarketConfig, Realm, RealmAuctionProvider,
    RealmId, RealmSnapshot, Region, Snapshot,
};
use app_integrations::{
    BlizzardAuctions, BlizzardItems, BlizzardRealms, DiscordWebhook, PerUserDiscord,
};
use cluster_core::Millis;
use cluster_local::SystemClock;
use storage::{SqliteUsers, SqliteWatches};

/// The commodity source, present or not.
pub enum Commodities {
    Live(Box<BlizzardAuctions<SystemClock>>),
    /// No `BLIZZARD_CLIENT_ID` / `BLIZZARD_CLIENT_SECRET` in the environment.
    Unconfigured,
}

impl CommodityProvider for Commodities {
    fn provider_name(&self) -> &'static str {
        match self {
            Commodities::Live(inner) => inner.provider_name(),
            Commodities::Unconfigured => "unconfigured",
        }
    }

    fn is_configured(&self) -> bool {
        matches!(self, Commodities::Live(_))
    }

    async fn commodities(
        &self,
        region: Region,
        wanted: &[ItemId],
        if_modified_since: Option<Millis>,
    ) -> AppResult<Snapshot> {
        match self {
            Commodities::Live(inner) => inner.commodities(region, wanted, if_modified_since).await,
            Commodities::Unconfigured => Err(AppError::Integration(
                "Battle.net credentials are not configured: set BLIZZARD_CLIENT_ID and \
                 BLIZZARD_CLIENT_SECRET"
                    .into(),
            )),
        }
    }
}

/// The per-realm gear source. Same credentials as [`Commodities`], different
/// endpoint and a price that means something different.
pub enum RealmAuctions {
    Live(Box<BlizzardRealms<SystemClock>>),
    Unconfigured,
}

impl RealmAuctionProvider for RealmAuctions {
    fn provider_name(&self) -> &'static str {
        match self {
            RealmAuctions::Live(inner) => inner.provider_name(),
            RealmAuctions::Unconfigured => "unconfigured",
        }
    }

    fn is_configured(&self) -> bool {
        matches!(self, RealmAuctions::Live(_))
    }

    async fn auctions(
        &self,
        region: Region,
        realm: RealmId,
        wanted: &[ItemId],
        if_modified_since: Option<Millis>,
    ) -> AppResult<RealmSnapshot> {
        match self {
            RealmAuctions::Live(inner) => {
                inner
                    .auctions(region, realm, wanted, if_modified_since)
                    .await
            }
            RealmAuctions::Unconfigured => Err(unconfigured()),
        }
    }

    async fn realms(&self, region: Region, wanted: &[RealmId]) -> AppResult<Vec<Realm>> {
        match self {
            RealmAuctions::Live(inner) => inner.realms(region, wanted).await,
            RealmAuctions::Unconfigured => Err(unconfigured()),
        }
    }
}

fn unconfigured() -> AppError {
    AppError::Integration(
        "Battle.net credentials are not configured: set BLIZZARD_CLIENT_ID and \
         BLIZZARD_CLIENT_SECRET"
            .into(),
    )
}

/// Static item data for tooltips. Shares the credential check with
/// [`Commodities`]: one set of Battle.net credentials serves both.
pub enum Items {
    Live(Box<BlizzardItems<SystemClock>>),
    Unconfigured,
}

impl ItemDetailProvider for Items {
    fn provider_name(&self) -> &'static str {
        match self {
            Items::Live(inner) => inner.provider_name(),
            Items::Unconfigured => "unconfigured",
        }
    }

    fn is_configured(&self) -> bool {
        matches!(self, Items::Live(_))
    }

    async fn tooltips(&self, region: Region, item: ItemId) -> AppResult<LocalizedTooltips> {
        match self {
            Items::Live(inner) => inner.tooltips(region, item).await,
            Items::Unconfigured => Err(AppError::Integration(
                "Battle.net credentials are not configured: set BLIZZARD_CLIENT_ID and \
                 BLIZZARD_CLIENT_SECRET"
                    .into(),
            )),
        }
    }
}

/// Outbound alert channels. Alerts are always stored and shown in the UI;
/// these are the extra push -- an optional instance-wide ops channel, and
/// self-service per-user channels for whoever configured one and is
/// watching the market that just fired. Neither depends on the other being
/// configured.
pub struct Alerts {
    global: Option<Box<DiscordWebhook>>,
    per_user: PerUserDiscord<SqliteWatches, SqliteUsers>,
}

impl Alerts {
    pub fn new(
        global: Option<Box<DiscordWebhook>>,
        per_user: PerUserDiscord<SqliteWatches, SqliteUsers>,
    ) -> Self {
        Self { global, per_user }
    }
}

impl AlertSink for Alerts {
    async fn publish(&self, alert: &Alert, item_name: &str) {
        if let Some(hook) = &self.global {
            hook.publish(alert, item_name).await;
        }
        self.per_user.publish(alert, item_name).await;
    }
}

/// Build the market configuration from CLI settings.
pub fn config(
    regions: Vec<Region>,
    realms: Vec<(Region, RealmId)>,
    interval_minutes: u64,
    realm_intervals_minutes: (u64, u64),
    retain_days: u64,
    downsample_days: u64,
    ladder_days: u64,
) -> MarketConfig {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    const MINUTE_MS: u64 = 60 * 1000;
    let (realm_min_interval_minutes, realm_max_interval_minutes) = realm_intervals_minutes;
    let realm_min_interval_ms = realm_min_interval_minutes.max(1) * MINUTE_MS;
    MarketConfig {
        regions,
        realms,
        collect_interval_ms: interval_minutes.max(1) * 60 * 1000,
        realm_min_interval_ms,
        realm_max_interval_ms: realm_max_interval_minutes.max(realm_min_interval_minutes.max(1))
            * MINUTE_MS,
        // Zero means forever in all three cases, and must survive as zero.
        retain_ms: retain_days * DAY_MS,
        downsample_after_ms: downsample_days * DAY_MS,
        ladder_hot_ms: ladder_days * DAY_MS,
        ..MarketConfig::default()
    }
}
