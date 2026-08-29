//! The recurring collection loop.
//!
//! The cluster runs jobs on demand and has no notion of a schedule, so this is
//! a plain periodic task rather than a cluster job -- which is also the honest
//! shape for the work: a commodity snapshot is *one* request per region per
//! hour, so there is nothing to fan out. That changes the moment we add
//! non-commodity items, which are per connected realm and mean roughly 250
//! independent fetches; that is when this becomes cluster work.

use std::time::Duration;

use app_core::Ports;
use app_core::item::ItemDetailProvider;
use app_core::locale::{ALL_LOCALES, DEFAULT_LOCALE};
use app_core::market::{Collector, Outcome, RealmAuctionProvider, RealmSnapshot, summarise_realm};
use app_core::repo::{PriceRepository, RealmPriceRepository, Store};
use app_core::service::{Freshness, ItemTooltipService};
use cluster_core::Millis;

pub fn spawn<E: Ports>(env: E) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(env))
}

async fn run<E: Ports>(env: E) {
    let market = env.market().clone();
    let mut ticker = tokio::time::interval(Duration::from_millis(
        market.collect_interval_ms.max(60_000),
    ));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    match env.active_catalog() {
        Some(catalog) => tracing::info!(
            regions = ?market.regions.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            every_minutes = market.collect_interval_ms / 60_000,
            items = catalog.tracked_ids().len(),
            expansion = %catalog.expansion,
            season = %catalog.season,
            archived = env.catalogs().catalogs.len() - 1,
            "auction house tracker started"
        ),
        None => tracing::warn!(
            "no expansion is marked active: nothing will be collected, archives stay readable"
        ),
    }

    name_realms(&env).await;

    loop {
        ticker.tick().await;
        collect_once(&env).await;
        collect_realms(&env).await;
        warm_tooltips(&env).await;
        prune(&env).await;
    }
}

async fn collect_once<E: Ports>(env: &E) {
    let market = env.market();
    // Archived expansions are never collected: that is what makes them frozen.
    let Some(catalog) = env.active_catalog() else {
        return;
    };
    let collector = Collector::new(
        env.commodities(),
        env.store().prices(),
        env.alert_sink(),
        catalog,
        market.rule,
    );

    for region in &market.regions {
        match collector.collect(*region, env.now()).await {
            Ok(report) => match report.outcome {
                Outcome::NotModified => {
                    tracing::debug!(region = %region, "commodity snapshot unchanged")
                }
                Outcome::AlreadyRecorded => {
                    tracing::debug!(region = %region, "commodity snapshot already stored")
                }
                Outcome::NoActiveCatalog => {}
                Outcome::Collected {
                    samples,
                    written,
                    alerts,
                } => tracing::info!(
                    region = %region,
                    samples,
                    written,
                    alerts = alerts.len(),
                    "collected commodity prices"
                ),
            },
            // A failed collection must not kill the loop: the next tick will
            // try again, and a transient 429 or 503 is expected.
            Err(e) => tracing::warn!(region = %region, error = %e, "price collection failed"),
        }
    }
}

/// Gear prices, one connected realm at a time.
///
/// A plain loop rather than cluster work, at six realms. It is deliberately
/// shaped so each realm is an independent unit -- its own timestamp, its own
/// failure -- because the moment this covers every realm it becomes 175
/// independent fetches, which is a job with one task per realm and exactly
/// what the scheduler upstairs is for.
async fn collect_realms<E: Ports>(env: &E) {
    let market = env.market();
    if market.realms.is_empty() || !env.realm_auctions().is_configured() {
        return;
    }
    let Some(catalog) = env.active_catalog() else {
        return;
    };
    let wanted = catalog.realm_tracked_ids();
    if wanted.is_empty() {
        return;
    }

    let prices = env.store().realm_prices();
    for (region, realm) in &market.realms {
        // Per realm, because realms regenerate on their own schedules: one
        // region-wide timestamp would re-fetch realms that had not moved and
        // skip ones that had.
        let since = match prices.last_observed(*region, *realm).await {
            Ok(at) => at,
            Err(e) => {
                tracing::warn!(region = %region, realm = %realm, error = %e,
                    "could not read the last realm snapshot; fetching in full");
                None
            }
        };

        match env
            .realm_auctions()
            .auctions(*region, *realm, &wanted, since)
            .await
        {
            Ok(RealmSnapshot::NotModified) => {
                tracing::debug!(region = %region, realm = %realm, "realm snapshot unchanged")
            }
            Ok(RealmSnapshot::Fresh {
                generated_at,
                listings,
            }) => {
                let found = listings.len();
                let samples = summarise_realm(listings, *region, *realm, generated_at);
                match prices.record_samples(&samples).await {
                    Ok(written) => tracing::info!(
                        region = %region,
                        realm = %realm,
                        listings = found,
                        variants = samples.len(),
                        written,
                        "collected gear prices"
                    ),
                    Err(e) => {
                        tracing::warn!(region = %region, realm = %realm, error = %e,
                            "storing gear prices failed")
                    }
                }
            }
            // One realm failing must not stop the others: they are separate
            // markets and separate requests.
            Err(e) => {
                tracing::warn!(region = %region, realm = %realm, error = %e,
                    "gear collection failed")
            }
        }
    }
}

/// Learn the configured realms' names, once, at startup.
///
/// Stored rather than looked up per request so the UI can say "Draenor"
/// without an upstream call, and so a realm dropped from the configuration
/// keeps its history readable instead of showing a bare number.
async fn name_realms<E: Ports>(env: &E) {
    let market = env.market();
    if market.realms.is_empty() || !env.realm_auctions().is_configured() {
        return;
    }
    let prices = env.store().realm_prices();
    for region in &market.regions {
        let wanted: Vec<_> = market
            .realms
            .iter()
            .filter(|(r, _)| r == region)
            .map(|(_, realm)| *realm)
            .collect();
        if wanted.is_empty() {
            continue;
        }
        match env.realm_auctions().realms(*region, &wanted).await {
            Ok(realms) => {
                for realm in &realms {
                    if let Err(e) = prices.record_realm(realm).await {
                        tracing::warn!(realm = %realm.id, error = %e, "could not store realm name");
                    }
                }
                tracing::info!(
                    region = %region,
                    realms = ?realms.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
                    "gear realms configured"
                );
            }
            Err(e) => {
                tracing::warn!(region = %region, error = %e,
                    "could not name the configured realms; the UI will show ids")
            }
        }
    }
}

/// Fetch the item tooltips that are missing from the cache.
///
/// Tooltips are cached for a week, so this normally does nothing at all --
/// but when it does, it means the pages can inline every tooltip and hovering
/// an icon never waits on a request.
///
/// One pass covers every region *and* every language: item text is the same
/// from every regional host, and one request returns all twelve languages. So
/// the cost is one call per item per week, against a budget measured in tens
/// of thousands per hour.
async fn warm_tooltips<E: Ports>(env: &E) {
    if !env.items().is_configured() {
        return;
    }
    let Some(catalog) = env.active_catalog() else {
        return;
    };
    // Any collected region serves the same text; the first is as good as any.
    let Some(region) = env.market().regions.first().copied() else {
        return;
    };

    let service = ItemTooltipService::new(
        env.items(),
        env.store().cache(),
        env.config().item_cache_ttl_ms,
    );

    let mut fetched = 0usize;
    let mut missing = 0usize;
    for entry in &catalog.items {
        for item in entry.item_ids() {
            let now = env.now();
            if service.cached(DEFAULT_LOCALE, item, now).await.is_some() {
                continue;
            }
            match service
                .lookup(region, DEFAULT_LOCALE, item, &entry.name, now)
                .await
                .1
            {
                // The upstream is unhappy. Stop rather than walk the whole
                // catalog failing; the next tick tries again.
                Freshness::Unavailable => {
                    tracing::warn!(
                        warmed = fetched,
                        "stopping tooltip warm-up after an upstream failure"
                    );
                    return;
                }
                // One id the game data has dropped says nothing about the next
                // one, and the placeholder is cached, so keep going.
                Freshness::Missing => missing += 1,
                _ => fetched += 1,
            }
            // Single-item calls against an endpoint we do not need in a hurry:
            // spacing them out keeps the burst off the budget.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    if fetched > 0 || missing > 0 {
        tracing::info!(
            items = fetched,
            missing,
            languages = ALL_LOCALES.len(),
            "warmed item tooltips"
        );
    }
}

async fn prune<E: Ports>(env: &E) {
    let retain = env.market().retain_ms;
    // Zero means keep everything, which is the default: the archive is the
    // product, and pruning would eat the oldest expansion first.
    if retain == 0 {
        return;
    }
    let cutoff = Millis(env.now().get().saturating_sub(retain));
    match env.store().prices().prune_before(cutoff).await {
        Ok(0) => {}
        Ok(rows) => tracing::info!(rows, "pruned expired price history"),
        Err(e) => tracing::warn!(error = %e, "could not prune price history"),
    }
}
