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
    ALGORITHM_VERSION, Collector, ItemId, ItemKind as ItemKindT, Outcome as CollectOutcome, Realm,
    RealmAuctionProvider, RealmCadence, RealmSnapshot, summarise_realm,
};
use app_core::repo::{
    KeyValueStore, PriceRepository, ReadModelRepository, RealmPriceRepository, SettingsRepository,
    Store, TokenPriceRepository,
};
use app_core::service::{Freshness, ItemTooltipService};
use app_integrations::blizzard::{BlizzardConfig, BlizzardCredentials, BlizzardWowToken};
use cluster_core::ClusterControl;
use cluster_core::Millis;

/// How long the boot rebuild waits for a worker to dial in.
///
/// **Found by running it.** A coordinator started with `--workers 0
/// --worker-listen` has nobody at all for the moment between binding the
/// socket and the first worker's `Hello`, and the archive rebuild starts in
/// that moment: it asked whether there was anybody to distribute to, was
/// correctly told no, and materialised 30,326 markets itself while two worker
/// processes connected five seconds later and sat idle. Every restart of the
/// web machine, on the deployment §15 describes.
///
/// A few seconds, once, at startup. It is not a fix for a cluster with no
/// workers -- that case still materialises locally, which is what §16 requires
/// -- it only stops "nobody has connected *yet*" being read as "nobody is
/// coming". The regular collection cycle needs none of this: it is half an
/// hour in, by which time a worker has either joined or is not going to.
const WORKER_GRACE: Duration = Duration::from_secs(5);
const TOKEN_INTERVAL: Duration = Duration::from_secs(20 * 60);

pub fn spawn<E: Ports>(
    env: E,
    artifacts: std::sync::Arc<crate::analysis_work::Artifacts>,
    awaits_workers: bool,
) -> tokio::task::JoinHandle<()>
where
    E::Clock: Clone,
{
    let tokens =
        BlizzardCredentials::from_env().and_then(|credentials| {
            match BlizzardWowToken::new(BlizzardConfig::default(), credentials, env.clock().clone())
            {
                Ok(tokens) => Some(tokens),
                Err(error) => {
                    tracing::warn!(%error, "could not build the WoW Token client");
                    None
                }
            }
        });
    tokio::spawn(run(env, artifacts, awaits_workers, tokens))
}

async fn run<E: Ports>(
    env: E,
    artifacts: std::sync::Arc<crate::analysis_work::Artifacts>,
    awaits_workers: bool,
    tokens: Option<BlizzardWowToken<E::Clock>>,
) where
    E::Clock: Clone,
{
    // The token endpoint is independent of collecting the auction house.  A
    // JoinSet keeps both tasks under this returned handle: aborting the
    // collector during shutdown drops the set and aborts both children.
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(run_market(env.clone(), artifacts, awaits_workers));
    if let Some(client) = tokens {
        tasks.spawn(run_tokens(env, client));
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "collector task ended unexpectedly");
        }
    }
}

async fn run_market<E: Ports>(
    env: E,
    artifacts: std::sync::Arc<crate::analysis_work::Artifacts>,
    awaits_workers: bool,
) {
    let market = env.market().clone();
    let mut commodity_ticker = tokio::time::interval(Duration::from_millis(
        market.collect_interval_ms.max(60_000),
    ));
    commodity_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut realm_ticker = tokio::time::interval(Duration::from_millis(
        market.realm_min_interval_ms.max(60_000),
    ));
    realm_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    match env.active_catalog() {
        Some(catalog) => tracing::info!(
            regions = ?market.regions.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            commodity_every_minutes = market.collect_interval_ms / 60_000,
            realm_min_minutes = market.realm_min_interval_ms / 60_000,
            realm_max_minutes = market.realm_max_interval_ms / 60_000,
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
    backfill(&env, &artifacts, awaits_workers).await;

    loop {
        tokio::select! {
            _ = commodity_ticker.tick() => {
                // Commodity data remains on its fixed cadence: §16 measured it
                // changing on 86.7% of observations, unlike quiet realm data.
                let collected = collect_once(&env).await;
                crate::materialise_task::publish(&env, &artifacts, &collected, &[]).await;
                warm_tooltips(&env).await;
                build_daily_rollups(&env).await;
                prune(&env).await;
                prune_ladders(&env).await;
            }
            _ = realm_ticker.tick() => {
                // A realm's own persisted schedule decides whether it is due.
                // When one moves, the whole affected region is recalculated:
                // the roll-up a card reads spans its connected realms.
                let realms = if collect_realms(&env).await {
                    env.market().regions.clone()
                } else {
                    Vec::new()
                };
                crate::materialise_task::publish(&env, &artifacts, &[], &realms).await;
            }
        }
    }
}

async fn run_tokens<E: Ports>(env: E, client: BlizzardWowToken<E::Clock>)
where
    E::Clock: Clone,
{
    let mut ticker = tokio::time::interval(TOKEN_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        collect_tokens(&env, &client).await;
    }
}

async fn collect_tokens<E: Ports>(env: &E, client: &BlizzardWowToken<E::Clock>)
where
    E::Clock: Clone,
{
    for region in &env.market().regions {
        match client.price(*region).await {
            Ok(token) => match env.store().prices().record(&token).await {
                Ok(true) => {
                    tracing::info!(region = %region, price = %token.price, "collected WoW Token price")
                }
                Ok(false) => tracing::debug!(region = %region, "WoW Token price already recorded"),
                Err(error) => {
                    tracing::warn!(region = %region, %error, "could not store WoW Token price")
                }
            },
            Err(error) => tracing::warn!(region = %region, %error, "WoW Token collection failed"),
        }
    }
}

/// Materialise the archive that is already here, once.
///
/// Without this, the first start after deploying Phase 2 serves empty windows
/// on every page: the archive is the product, and it would look like it had
/// been thrown away. It runs only when there is nothing worth serving, so it
/// is a one-off rather than a cost on every boot.
///
/// **"Nothing worth serving" includes a version built by an older algorithm**,
/// and that is what [`ALGORITHM_VERSION`] is for. Phase 5 added columns to
/// every window -- the percentiles, the band, the evidence, the sparkline --
/// and a row written by Phase 2 has none of them. It is a legal row and it is
/// published, so the old check would have gone on serving it: cards with no
/// verdict and no line on them until the next collection cycle happened to
/// recalculate that market, which for a region that fetches unchanged is
/// never. An upgrade that quietly serves less than it did is worse than one
/// that pauses to rebuild, so it rebuilds.
async fn backfill<E: Ports>(
    env: &E,
    artifacts: &std::sync::Arc<crate::analysis_work::Artifacts>,
    awaits_workers: bool,
) {
    let why = match env.store().read_model().published().await {
        Ok(Some(version)) if version.algorithm >= ALGORITHM_VERSION => {
            tracing::info!(
                version = version.version,
                markets = version.markets,
                algorithm = version.algorithm,
                "serving the published market analysis"
            );
            return;
        }
        Ok(Some(version)) => {
            tracing::info!(
                version = version.version,
                was = version.algorithm,
                now = ALGORITHM_VERSION,
                "the published analysis predates this binary's definitions: rebuilding"
            );
            "the definitions moved"
        }
        Ok(None) => "no published analysis yet",
        Err(error) => {
            tracing::warn!(%error, "could not read the published analysis version");
            return;
        }
    };

    if awaits_workers {
        wait_for_a_worker(env).await;
    }

    let regions = env.market().regions.clone();
    tracing::info!(
        regions = ?regions.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        why,
        "materialising the existing archive"
    );
    // The previous version stays published throughout, which is the point of
    // staging: a rebuild that takes four seconds is four seconds of the old
    // analysis, not four seconds of an empty site.
    crate::materialise_task::publish(env, artifacts, &regions, &regions).await;
}

/// Give a worker [`WORKER_GRACE`] to arrive before deciding there are none.
///
/// Only on a deployment that is listening for them -- a coordinator with
/// in-process workers already has its nodes when this runs, and one with
/// neither is a server with no cluster and nothing to wait for.
async fn wait_for_a_worker<E: Ports>(env: &E) {
    let deadline = tokio::time::Instant::now() + WORKER_GRACE;
    while tokio::time::Instant::now() < deadline {
        if !env.cluster().nodes().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tracing::info!(
        seconds = WORKER_GRACE.as_secs(),
        "no worker connected in time; materialising the archive here"
    );
}

/// Collect every region's commodity snapshot.
///
/// Returns the regions whose snapshot actually changed, which is what the
/// materialiser needs: recalculating a region the upstream said was unchanged
/// would be work with a guaranteed identical result.
async fn collect_once<E: Ports>(env: &E) -> Vec<app_core::market::Region> {
    let market = env.market();
    // Archived expansions are never collected: that is what makes them frozen.
    let Some(catalog) = env.active_catalog() else {
        return Vec::new();
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

    let mut changed = Vec::new();
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
                    ladders,
                    alerts,
                } => {
                    changed.push(*region);
                    tracing::info!(
                        region = %region,
                        samples,
                        written,
                        ladders,
                        alerts = alerts.len(),
                        "collected commodity prices"
                    );
                }
            },
            // A failed collection must not kill the loop: the next tick will
            // try again, and a transient 429 or 503 is expected.
            Err(e) => tracing::warn!(region = %region, error = %e, "price collection failed"),
        }
    }
    changed
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
/// Collect each enabled realm whose own cadence says it is due.
///
/// Returns whether anything actually changed, which is what decides if the
/// roll-ups need rebuilding.
async fn collect_realms<E: Ports>(env: &E) -> bool {
    if !env.realm_auctions().is_configured() {
        return false;
    }
    let Some(catalog) = env.active_catalog() else {
        return false;
    };
    let disabled = disabled_kinds(env).await;
    let wanted: Vec<ItemId> = catalog
        .items
        .iter()
        .filter(|i| !i.kind.is_commodity() && !disabled.contains(&i.kind.as_str().to_string()))
        .flat_map(|i| i.item_ids())
        .collect();
    if wanted.is_empty() {
        return false;
    }

    // The store is the source of truth for which realms to collect, not the
    // flag: the admin page changes it while the server runs.
    let realms: Vec<Realm> = match env.store().realm_prices().realms().await {
        Ok(realms) => realms.into_iter().filter(|r| r.enabled).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not read the realm list");
            return false;
        }
    };
    if realms.is_empty() {
        return false;
    }

    let now = env.now();
    let market = env.market();
    let mut due = Vec::new();
    for realm in realms {
        let cadence = match load_realm_cadence(env, &realm).await {
            Ok(Some(cadence)) => {
                cadence.within_bounds(market.realm_min_interval_ms, market.realm_max_interval_ms)
            }
            Ok(None) => RealmCadence::new(now, market.realm_min_interval_ms),
            Err(error) => {
                // Missing cadence metadata is never a reason to starve a
                // realm; a safe retry at the minimum is better than stale UI.
                tracing::warn!(realm = %realm.name, %error,
                    "could not read realm cadence; collecting at the minimum interval");
                RealmCadence::new(now, market.realm_min_interval_ms)
            }
        };
        if !cadence.is_due(now) {
            continue;
        }
        due.push((realm, cadence));
    }
    if due.is_empty() {
        return false;
    }

    let width = fan_out(env).await;
    let started = std::time::Instant::now();
    let mut queue = due.into_iter();
    let mut running = tokio::task::JoinSet::new();
    let (mut collected, mut unchanged, mut failed) = (0u32, 0u32, 0u32);

    loop {
        // Top the set back up to the cluster's width, then wait for one to
        // finish. A fixed-size window rather than a batch: a slow realm holds
        // up one slot instead of the whole cycle.
        while running.len() < width {
            let Some((realm, cadence)) = queue.next() else {
                break;
            };
            let env = env.clone();
            let wanted = wanted.clone();
            running.spawn(async move { collect_one_realm(&env, &realm, cadence, &wanted).await });
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
    collected > 0
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

async fn collect_one_realm<E: Ports>(
    env: &E,
    realm: &Realm,
    cadence: RealmCadence,
    wanted: &[ItemId],
) -> Outcome {
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
        Ok(RealmSnapshot::NotModified) => {
            save_realm_cadence(
                env,
                realm,
                cadence.after_unchanged(
                    env.now(),
                    env.market().realm_min_interval_ms,
                    env.market().realm_max_interval_ms,
                ),
            )
            .await;
            Outcome::Unchanged
        }
        Ok(RealmSnapshot::Fresh {
            generated_at,
            listings,
        }) => {
            let (samples, ladders) =
                summarise_realm(listings, realm.region, realm.id, generated_at);
            match prices
                .record_snapshot(&samples, realm.region, realm.id, generated_at, &ladders)
                .await
            {
                Ok((written, rungs)) => {
                    save_realm_cadence(
                        env,
                        realm,
                        RealmCadence::after_activity(
                            env.now(),
                            env.market().realm_min_interval_ms,
                            env.market().realm_max_interval_ms,
                        ),
                    )
                    .await;
                    tracing::debug!(realm = %realm.name, written, rungs,
                        "collected gear prices");
                    Outcome::Collected
                }
                Err(e) => {
                    tracing::warn!(realm = %realm.name, error = %e, "storing gear snapshot failed");
                    save_realm_cadence(
                        env,
                        realm,
                        cadence.after_failure(
                            env.now(),
                            env.market().realm_min_interval_ms,
                            env.market().realm_max_interval_ms,
                        ),
                    )
                    .await;
                    Outcome::Failed
                }
            }
        }
        // One realm failing must not stop the others: they are separate
        // markets and separate requests.
        Err(e) => {
            tracing::warn!(realm = %realm.name, error = %e, "gear collection failed");
            save_realm_cadence(
                env,
                realm,
                cadence.after_failure(
                    env.now(),
                    env.market().realm_min_interval_ms,
                    env.market().realm_max_interval_ms,
                ),
            )
            .await;
            Outcome::Failed
        }
    }
}

fn realm_cadence_key(realm: &Realm) -> String {
    format!("market/realm-cadence/{}/{}", realm.region, realm.id)
}

async fn load_realm_cadence<E: Ports>(
    env: &E,
    realm: &Realm,
) -> app_core::error::RepoResult<Option<RealmCadence>> {
    let Some(value) = env.store().kv().get(&realm_cadence_key(realm)).await? else {
        return Ok(None);
    };
    let parsed = std::str::from_utf8(&value)
        .ok()
        .and_then(|value| value.split_once(','))
        .and_then(|(interval_ms, next_check_at)| {
            Some((interval_ms.parse().ok()?, next_check_at.parse().ok()?))
        });
    match parsed {
        Some((interval_ms, next_check_at)) => Ok(Some(RealmCadence {
            interval_ms,
            next_check_at: Millis(next_check_at),
        })),
        None => {
            tracing::warn!(realm = %realm.name, "ignoring invalid persisted realm cadence");
            Ok(None)
        }
    }
}

async fn save_realm_cadence<E: Ports>(env: &E, realm: &Realm, cadence: RealmCadence) {
    let value = format!("{},{}", cadence.interval_ms, cadence.next_check_at.get());
    if let Err(error) = env
        .store()
        .kv()
        .put(&realm_cadence_key(realm), value.as_bytes())
        .await
    {
        tracing::warn!(realm = %realm.name, %error, "could not persist realm cadence");
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

/// Compute perpetual daily read models for finished days.
async fn build_daily_rollups<E: Ports>(env: &E) {
    // Compute up to yesterday (the most recently finished day).
    let yesterday = ((env.now().get() / 86400000) - 1) * 86400000;

    match env
        .store()
        .prices()
        .build_daily_rollups(Millis(yesterday))
        .await
    {
        Ok(0) => {}
        Ok(rows) => tracing::info!(rows, "built commodity daily rollups"),
        Err(e) => tracing::warn!(error = %e, "could not build commodity rollups"),
    }
    match env
        .store()
        .realm_prices()
        .build_daily_rollups(Millis(yesterday))
        .await
    {
        Ok(0) => {}
        Ok(rows) => tracing::info!(rows, "built gear daily rollups"),
        Err(e) => tracing::warn!(error = %e, "could not build gear rollups"),
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

/// Drop ladders that have left the hot window.
///
/// Separate from `prune` because the policies are different by design, and the
/// difference is the point: price history defaults to *keep forever* because
/// the archive is the product, while ladders default to a fortnight because
/// they are every rung of every market and the archive would become them.
///
/// This runs whatever `retain_ms` says. A deployment keeping its price history
/// forever -- which is the default -- still has to bound its ladders, or the
/// one policy silently disables the other.
async fn prune_ladders<E: Ports>(env: &E) {
    let hot = env.market().ladder_hot_ms;
    // Zero means keep every ladder for ever. Honest, and it will need a disk.
    if hot == 0 {
        return;
    }
    let cutoff = Millis(env.now().get().saturating_sub(hot));
    let commodity = env.store().prices().prune_ladders_before(cutoff).await;
    let realm = env
        .store()
        .realm_prices()
        .prune_ladders_before(cutoff)
        .await;
    match (commodity, realm) {
        (Ok(0), Ok(0)) => {}
        (Ok(a), Ok(b)) => {
            tracing::info!(
                commodity = a,
                realm = b,
                "pruned ladders past the hot window"
            )
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(error = %e, "could not prune ladders")
        }
    }
}
