//! One collection pass: fetch, summarise, store, alert.
//!
//! Runs hourly per region. Deliberately idempotent -- a snapshot that has
//! already been recorded produces no new rows and, importantly, no repeat
//! alerts, so a retried job is harmless.

use std::collections::BTreeMap;
use std::future::Future;

use cluster_core::Millis;

use crate::error::AppResult;
use crate::market::{
    Alert, AlertRule, Catalog, CommodityProvider, Copper, ItemId, Listing, PriceSample, Region,
    Snapshot, alerts, summarise,
};
use crate::repo::PriceRepository;

/// Where an alert goes once it has been raised. The UI reads alerts back out
/// of storage; this is for pushing them somewhere outbound.
pub trait AlertSink: Send + Sync + 'static {
    fn publish(&self, alert: &Alert, item_name: &str) -> impl Future<Output = ()> + Send;
}

/// A sink that does nothing, for when no channel is configured.
pub struct NullSink;

impl AlertSink for NullSink {
    async fn publish(&self, _alert: &Alert, _item_name: &str) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Upstream had nothing new. The common case: we poll more often than the
    /// hourly snapshot changes.
    NotModified,
    /// No expansion is marked active, so there is nothing to collect. Reached
    /// after an expansion is archived and before the next is added.
    NoActiveCatalog,
    /// This exact snapshot was already stored -- a retry, or a restart inside
    /// the same hour.
    AlreadyRecorded,
    Collected {
        samples: usize,
        written: u64,
        alerts: Vec<Alert>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub region: Region,
    pub generated_at: Millis,
    pub outcome: Outcome,
}

pub struct Collector<'a, P, R, S> {
    provider: &'a P,
    prices: &'a R,
    sink: &'a S,
    catalog: &'a Catalog,
    rule: AlertRule,
}

impl<'a, P, R, S> Collector<'a, P, R, S>
where
    P: CommodityProvider,
    R: PriceRepository,
    S: AlertSink,
{
    pub fn new(
        provider: &'a P,
        prices: &'a R,
        sink: &'a S,
        catalog: &'a Catalog,
        rule: AlertRule,
    ) -> Self {
        Self {
            provider,
            prices,
            sink,
            catalog,
            rule,
        }
    }

    pub async fn collect(&self, region: Region, now: Millis) -> AppResult<Report> {
        let tracked = self.catalog.tracked_ids();
        let since = self.prices.last_observed(region).await?;

        let snapshot = self.provider.commodities(region, &tracked, since).await?;
        let (generated_at, listings) = match snapshot {
            Snapshot::NotModified => {
                return Ok(Report {
                    region,
                    generated_at: since.unwrap_or(now),
                    outcome: Outcome::NotModified,
                });
            }
            Snapshot::Fresh {
                generated_at,
                listings,
            } => {
                // A missing Last-Modified is survivable but must not stamp the
                // sample at the epoch.
                let at = if generated_at.get() == 0 {
                    now
                } else {
                    generated_at
                };
                (at, listings)
            }
        };

        let samples = self.summarise_all(region, generated_at, listings);
        let written = self.prices.record_samples(&samples).await?;

        // Zero rows written means every one of them collided with an existing
        // primary key: we have seen this snapshot before. Alerting again would
        // spam on every restart.
        if written == 0 && !samples.is_empty() {
            return Ok(Report {
                region,
                generated_at,
                outcome: Outcome::AlreadyRecorded,
            });
        }

        let alerts = self.raise_alerts(&samples, generated_at).await?;

        Ok(Report {
            region,
            generated_at,
            outcome: Outcome::Collected {
                samples: samples.len(),
                written,
                alerts,
            },
        })
    }

    /// Group listings by item and reduce each group to one observation.
    fn summarise_all(
        &self,
        region: Region,
        at: Millis,
        listings: Vec<Listing>,
    ) -> Vec<PriceSample> {
        let mut by_item: BTreeMap<ItemId, Vec<Listing>> = BTreeMap::new();
        for listing in listings {
            by_item.entry(listing.item).or_default().push(listing);
        }

        by_item
            .into_iter()
            .filter_map(|(item, mut group)| {
                summarise(&mut group).map(|stats| stats.into_sample(item, region, at))
            })
            .collect()
    }

    async fn raise_alerts(&self, samples: &[PriceSample], now: Millis) -> AppResult<Vec<Alert>> {
        let mut raised = Vec::new();
        let index = self.catalog.index();

        for sample in samples {
            let Some(entry) = index.get(&sample.item) else {
                continue;
            };

            // Cooldown first: it is the cheapest check and skips a history read.
            if let Some(last) = self
                .prices
                .last_alert_at(sample.item, sample.region)
                .await?
                && now.since(last) < self.rule.cooldown_ms
            {
                continue;
            }

            let window_start = Millis(now.get().saturating_sub(self.rule.lookback_ms));
            let history = self
                .prices
                .history(sample.item, sample.region, window_start)
                .await?;

            // The sample we just stored is in there; it must not be part of
            // its own baseline.
            let baseline: Vec<PriceSample> = history
                .into_iter()
                .filter(|s| s.observed_at != sample.observed_at)
                .collect();

            let floor = entry.floor_copper.map(Copper);
            let Some(alert) = alerts::evaluate(&self.rule, sample, &baseline, floor) else {
                continue;
            };

            self.prices.record_alert(&alert).await?;
            let name = entry.display_name(sample.item);
            tracing::info!(
                item = %sample.item,
                region = %sample.region,
                severity = alert.severity.as_str(),
                "{}",
                alert.headline(&name)
            );
            self.sink.publish(&alert, &name).await;
            raised.push(alert);
        }

        Ok(raised)
    }
}
