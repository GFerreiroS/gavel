//! Wiring for the auction-house tracker.
//!
//! Both the data source and the notification channel are optional: the app has
//! to start and be useful without Battle.net credentials or a Discord webhook.
//! Rather than making them `Option` everywhere, each has an "unconfigured"
//! variant that reports the problem at the point of use.

use app_core::error::{AppError, AppResult};
use app_core::market::{
    Alert, AlertSink, CommodityProvider, ItemId, MarketConfig, Region, Snapshot,
};
use app_integrations::{BlizzardAuctions, DiscordWebhook};
use cluster_core::Millis;
use cluster_local::SystemClock;

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

/// Outbound alert channel. Alerts are always stored and shown in the UI; this
/// is the extra push.
pub enum Alerts {
    Discord(Box<DiscordWebhook>),
    None,
}

impl AlertSink for Alerts {
    async fn publish(&self, alert: &Alert, item_name: &str) {
        match self {
            Alerts::Discord(hook) => hook.publish(alert, item_name).await,
            Alerts::None => {}
        }
    }
}

/// Build the market configuration from CLI settings.
pub fn config(regions: Vec<Region>, interval_minutes: u64, retain_days: u64) -> MarketConfig {
    MarketConfig {
        regions,
        collect_interval_ms: interval_minutes.max(1) * 60 * 1000,
        retain_ms: retain_days.max(1) * 24 * 60 * 60 * 1000,
        ..MarketConfig::default()
    }
}
