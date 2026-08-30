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
use app_core::market::{
    Collector, ItemId, ItemKind as ItemKindT, Outcome as CollectOutcome, Realm,
    RealmAuctionProvider, RealmSnapshot, summarise_realm,
};
use app_core::repo::{PriceRepository, RealmPriceRepository, SettingsRepository, Store};
use app_core::service::{Freshness, ItemTooltipService};
use cluster_core::ClusterControl;
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
            season = %catalog.season_label(),
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
        downsample(&env).await;
        prune(&env).await;
    }
}

async fn collect_once<E: Ports>(env: &E) {
    let market = env.market();
    // Archived expansions are never collected: that is what makes them frozen.
    let Some(catalog) = env.active_catalog() else {
        return;
    };
    // Categories an administrator has switched off are not stored. The
    // snapshot is one request for the region either way, so this is about
    // what the pages show, not about what the upstream is asked for.
    let skip: Vec<ItemKindT> = disabled_kinds(env)
        .await
        .iter()
        .filter_map(|name| ItemKindT::ALL.into_iter().find(|k| k.as_str() == name))
        .collect();
    let collector = Collector::with_skipped(
        env.commodities(),
        env.store().prices(),
        env.alert_sink(),
        catalog,
        market.rule,
        &skip,
    );

    for region in &market.regions {
        match collector.collect(*region, env.now()).await {
            Ok(report) => match report.outcome {
                CollectOutcome::NotModified => {
                    tracing::debug!(region = %region, "commodity snapshot unchanged")
                }
                CollectOutcome::AlreadyRecorded => {
                    tracing::debug!(region = %region, "commodity snapshot already stored")
                }
                CollectOutcome::NoActiveCatalog => {}
                CollectOutcome::Collected {
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

/// Which categories an administrator has switched off.
///
/// Absent means on: a category added by a later release starts collected
/// rather than being ignored because nobody had a row for it.
async fn disabled_kinds<E: Ports>(env: &E) -> Vec<String> {
    match env.store().settings().disabled().await {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(error = %e, "could not read collection settings; collecting everything");
            Vec::new()
        }
    }
}

/// Gear prices, realm by realm, as many at a time as the cluster is wide.
///
/// One node collects them in sequence; five nodes collect five at a time. The
/// fetching itself stays on this process, and that is a deliberate limit
/// rather than an oversight: `cluster_core::workload::run_task` is documented
/// as pure -- no async, no platform calls -- which is what makes "the same
/// code runs in every worker" true, and a worker has neither Battle.net
/// credentials nor a database. Handing a realm fetch to a remote worker means
/// giving workers both, which is a change to the cluster's contract and not
/// one to make in passing.
///
/// What the cluster does decide today is *how much* work is in flight, which
/// is the part that makes 184 realms possible at all.
async fn collect_realms<E: Ports>(env: &E) {
    if !env.realm_auctions().is_configured() {
        return;
    }
    let Some(catalog) = env.active_catalog() else {
        return;
    };
    let disabled = disabled_kinds(env).await;
    let wanted: Vec<ItemId> = catalog
        .items
        .iter()
        .filter(|i| !i.kind.is_commodity() && !disabled.contains(&i.kind.as_str().to_string()))
        .flat_map(|i| i.item_ids())
        .collect();
    if wanted.is_empty() {
        return;
    }

    // The store is the source of truth for which realms to collect, not the
    // flag: the admin page changes it while the server runs.
    let realms: Vec<Realm> = match env.store().realm_prices().realms().await {
        Ok(realms) => realms.into_iter().filter(|r| r.enabled).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read the realm list");
            return;
        }
    };
    if realms.is_empty() {
        return;
    }

    let width = fan_out(env).await;
    let started = std::time::Instant::now();
    let mut queue = realms.into_iter();
    let mut running = tokio::task::JoinSet::new();
    let (mut collected, mut unchanged, mut failed) = (0u32, 0u32, 0u32);

    loop {
        // Top the set back up to the cluster's width, then wait for one to
        // finish. A fixed-size window rather than a batch: a slow realm holds
        // up one slot instead of the whole cycle.
        while running.len() < width {
            let Some(realm) = queue.next() else { break };
            let env = env.clone();
            let wanted = wanted.clone();
            running.spawn(async move { collect_one_realm(&env, &realm, &wanted).await });
        }
        let Some(finished) = running.join_next().await else {
            break;
        };
        match finished {
            Ok(Outcome::Collected) => collected += 1,
            Ok(Outcome::Unchanged) => unchanged += 1,
            Ok(Outcome::Failed) => failed += 1,
            // A panicking task must not take the cycle with it.
            Err(e) => {
                failed += 1;
                tracing::warn!(error = %e, "a realm collection task panicked");
            }
        }
    }

    tracing::info!(
        realms = collected + unchanged + failed,
        collected,
        unchanged,
        failed,
        in_flight = width,
        seconds = started.elapsed().as_secs_f32(),
        "gear collection cycle finished"
    );
}

/// What one realm's collection did, for the cycle summary.
enum Outcome {
    Collected,
    Unchanged,
    Failed,
}

/// How many realms to have in flight: one per node in the cluster.
///
/// A single-node instance collects in sequence, which is the honest shape for
/// one machine. Capped, because the far end is somebody else's API and the
/// budget is shared with every other request this process makes.
async fn fan_out<E: Ports>(env: &E) -> usize {
    const CAP: usize = 16;
    let nodes = env.cluster().snapshot().await.nodes_online;
    (nodes as usize).clamp(1, CAP)
}

async fn collect_one_realm<E: Ports>(env: &E, realm: &Realm, wanted: &[ItemId]) -> Outcome {
    let prices = env.store().realm_prices();
    // Per realm, because realms regenerate on their own schedules: one
    // region-wide timestamp would re-fetch realms that had not moved and skip
    // ones that had.
    let since = match prices.last_observed(realm.region, realm.id).await {
        Ok(at) => at,
        Err(e) => {
            tracing::warn!(realm = %realm.name, error = %e,
                "could not read the last realm snapshot; fetching in full");
            None
        }
    };

    match env
        .realm_auctions()
        .auctions(realm.region, realm.id, wanted, since)
        .await
    {
        Ok(RealmSnapshot::NotModified) => Outcome::Unchanged,
        Ok(RealmSnapshot::Fresh {
            generated_at,
            listings,
        }) => {
            let samples = summarise_realm(listings, realm.region, realm.id, generated_at);
            match prices.record_samples(&samples).await {
                Ok(written) => {
                    tracing::debug!(realm = %realm.name, written, "collected gear prices");
                    Outcome::Collected
                }
                Err(e) => {
                    tracing::warn!(realm = %realm.name, error = %e, "storing gear prices failed");
                    Outcome::Failed
                }
            }
        }
        // One realm failing must not stop the others: they are separate
        // markets and separate requests.
        Err(e) => {
            tracing::warn!(realm = %realm.name, error = %e, "gear collection failed");
            Outcome::Failed
        }
    }
}

/// Learn which realms exist, once, at startup.
///
/// With no realms configured this discovers every connected realm in every
/// collected region -- 184 of them across EU, US, KR and TW -- and records
/// them. Recording never overrides `enabled`, so a realm switched off in the
/// admin page stays off when discovery meets it again tomorrow.
async fn name_realms<E: Ports>(env: &E) {
    let market = env.market();
    if !env.realm_auctions().is_configured() {
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
        // An explicit list is honoured; otherwise every realm in the region.
        if !market.realms.is_empty() && wanted.is_empty() {
            continue;
        }
        match env.realm_auctions().realms(*region, &wanted).await {
            Ok(realms) => {
                for realm in &realms {
                    if let Err(e) = prices.record_realm(realm).await {
                        tracing::warn!(realm = %realm.name, error = %e,
                            "could not store realm name");
                    }
                }
                tracing::info!(region = %region, realms = realms.len(), "gear realms known");
            }
            Err(e) => {
                tracing::warn!(region = %region, error = %e,
                    "could not list the region's realms; using whatever is already stored")
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

/// Collapse old days into single rows.
///
/// Runs before pruning, and usually instead of it: keeping the archive at one
/// row per day is what makes "keep forever" affordable now that the catalogue
/// is hundreds of items across four regions and every connected realm.
async fn downsample<E: Ports>(env: &E) {
    let after = env.market().downsample_after_ms;
    if after == 0 {
        return;
    }
    let cutoff = Millis(env.now().get().saturating_sub(after));
    match env.store().prices().downsample_before(cutoff).await {
        Ok(0) => {}
        Ok(rows) => tracing::info!(rows, "downsampled commodity history to daily"),
        Err(e) => tracing::warn!(error = %e, "could not downsample commodity history"),
    }
    match env.store().realm_prices().downsample_before(cutoff).await {
        Ok(0) => {}
        Ok(rows) => tracing::info!(rows, "downsampled gear history to daily"),
        Err(e) => tracing::warn!(error = %e, "could not downsample gear history"),
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
