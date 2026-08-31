//! Recalculating the read model, and publishing it.
//!
//! CLAUDE.md §15: **calculate on update, read on request.** The collector
//! persists observations; this reduces them into the rows a page reads; and a
//! page reads the last complete version with its real timestamp, whatever this
//! is doing at the time.
//!
//! It runs in-process here, called by the collector after a snapshot lands.
//! Phase 4 moves the pure calculation to remote workers; that changes where
//! `market::materialise` runs, not what it produces and not who publishes it.
//! `cargo run` has to keep being the whole story, so the local path is the one
//! that is written first and stays.

use app_core::Ports;
use app_core::market::materialise::{self, ALGORITHM_VERSION, Materialised};
use app_core::market::window::Window;
use app_core::market::{Catalog, ItemId, PriceSample, Region};
use app_core::repo::{PriceRepository, ReadModelRepository, RealmPriceRepository, Store};
use cluster_core::Millis;
use std::collections::BTreeMap;

/// Markets staged per transaction.
///
/// A whole region in one transaction would hold SQLite's single writer for the
/// length of the rebuild, and the collector writing the next snapshot would
/// queue behind it. Small enough to interleave, large enough that the
/// per-transaction cost is not what dominates.
const BATCH: usize = 250;

/// The interval the per-realm pages are about.
///
/// Gear and recipes are collected far less densely than commodities -- a
/// market is caught by roughly one snapshot in three -- so a month is what
/// makes a chart a chart rather than a scatter of five points. It is also what
/// `routes::gear_stats` has always fetched, and this phase is not the place to
/// change what a page covers.
const REALM_WINDOW: Window = Window::Days(30);

/// Recalculate the markets in these regions and publish, in **one** version.
///
/// Commodities and per-realm markets together, deliberately. §15's rule is
/// that a page never sees a partial version, and publishing the two halves
/// separately would be a moment at which the consumables page had moved on and
/// the gear page had not -- which is the same fault as four regions arriving
/// one at a time, and it is the fault that let a benchmark measure a server
/// whose roll-ups were still being built.
///
/// Either list may be empty: a cycle where only the commodity snapshots moved
/// recalculates only those, and the per-realm rows keep what they had.
///
/// Failure abandons the candidate and leaves the published version exactly
/// where it was. That is the whole point of the staging state, and it is why
/// this returns `()` rather than propagating -- there is nothing for the
/// caller to do about it that is better than the next cycle trying again.
pub async fn publish<E: Ports>(env: &E, commodity: &[Region], per_realm: &[Region]) {
    // Every catalogue a visitor may see, not only the one being collected.
    //
    // Phase 9's third bullet -- "archived pages use their last published
    // analysis and can rebuild under a new algorithm from the retained
    // representation" -- is what forced this. Resolving the whole region's
    // history against the *active* catalogue was wrong in both directions the
    // moment a second catalogue existed: an archived tier's gear was re-keyed
    // by a catalogue that has never heard of its tracks, and its patch and
    // tier windows -- `Window::all_for` reads them off the catalogue -- were
    // simply not recalculated, so a rebuild under a new algorithm quietly
    // dropped every archived window it was supposed to reproduce.
    //
    // A market is therefore materialised by the catalogue that *owns* its
    // item, and an item nobody claims falls back to the live catalogue, which
    // is what the whole region used to do.
    let owners = env.public_owners();
    let public = env.public_catalogs();
    let Some(fallback) = env.active_catalog().or_else(|| public.first().copied()) else {
        return;
    };
    if commodity.is_empty() && per_realm.is_empty() {
        return;
    }

    let now = env.now();
    let read_model = env.store().read_model();

    let version = match read_model.begin(ALGORITHM_VERSION, now).await {
        Ok(version) => version,
        Err(error) => {
            tracing::warn!(%error, "could not open an analysis version");
            return;
        }
    };

    let started = std::time::Instant::now();
    let mut markets = 0u64;
    let mut oldest: Option<Millis> = None;
    let mut newest: Option<Millis> = None;

    let passes = commodity
        .iter()
        .map(|region| ("commodity", *region))
        .chain(per_realm.iter().map(|region| ("per-realm", *region)));

    for (kind, region) in passes {
        let outcome = match kind {
            "commodity" => commodity_region(env, &owners, fallback, region, version, now).await,
            _ => realm_region(env, &owners, &public, fallback, region, version, now).await,
        };
        match outcome {
            Ok(report) => {
                markets += report.markets;
                oldest = min_option(oldest, report.oldest);
                newest = max_option(newest, report.newest);
            }
            Err(error) => {
                tracing::warn!(%region, kind, %error, "materialisation failed");
                // Abandoning is not tidying up: it is what keeps the published
                // version whole. A half-staged candidate left alive would be
                // published by the next run as if it were complete.
                let note = format!("{kind} {region}: {error}");
                if let Err(error) = read_model.abandon(version, &note).await {
                    tracing::warn!(%error, version, "could not abandon the candidate");
                }
                return;
            }
        }
    }

    match read_model
        .publish(version, (oldest, newest), env.now())
        .await
    {
        Ok(()) => tracing::info!(
            version,
            markets,
            commodity_regions = commodity.len(),
            realm_regions = per_realm.len(),
            seconds = started.elapsed().as_secs_f32(),
            "published a market analysis version"
        ),
        Err(error) => {
            tracing::warn!(%error, version, "could not publish; the previous version stands");
            let _ = read_model
                .abandon(version, &format!("publish failed: {error}"))
                .await;
        }
    }
}

struct RegionReport {
    markets: u64,
    oldest: Option<Millis>,
    newest: Option<Millis>,
}

async fn commodity_region<E: Ports>(
    env: &E,
    owners: &BTreeMap<ItemId, &Catalog>,
    fallback: &Catalog,
    region: Region,
    version: u64,
    now: Millis,
) -> app_core::error::RepoResult<RegionReport> {
    // One query for the region rather than one per item. Ordered by market and
    // then by time, so the grouping below is a walk rather than a sort.
    let history = env.store().prices().history_in_region(region).await?;
    let oldest = history.first().map(|s| s.observed_at);
    let newest = history.iter().map(|s| s.observed_at).max();

    // The newest ladder of every market, in one query rather than one per
    // item -- the same reason `history_in_region` exists. Empty for a market
    // nothing has collected a ladder for yet, which is every market on an
    // archive gathered before Phase 7.
    let ladders: BTreeMap<ItemId, app_core::market::Ladder> = env
        .store()
        .prices()
        .latest_ladders(region)
        .await?
        .into_iter()
        .map(|(item, _, ladder)| (item, ladder))
        .collect();
    let no_ladder = app_core::market::Ladder::default();

    // One window list per catalogue rather than per market: they are the same
    // dozen strings for every item a catalogue owns, and building them 2,042
    // times would be §11b's linear-scan-inside-a-loop in another costume.
    let mut windows: BTreeMap<&str, Vec<Window>> = BTreeMap::new();
    for catalog in owners.values() {
        windows
            .entry(catalog.id.as_str())
            .or_insert_with(|| Window::all_for(catalog));
    }
    windows
        .entry(fallback.id.as_str())
        .or_insert_with(|| Window::all_for(fallback));

    let read_model = env.store().read_model();

    let mut markets = 0u64;
    let mut batch: Vec<Materialised> = Vec::with_capacity(BATCH);
    for group in grouped(&history) {
        // The catalogue that *owns* this item, not whichever one is active:
        // an archived tier's windows are read off the catalogue that declared
        // them, and re-keying its gear under the live one is how a rebuild
        // quietly drops the windows it was meant to reproduce.
        let catalog = owners.get(&group[0].item).copied().unwrap_or(fallback);
        let key = catalog.market_of(&group[0]);
        let ladder = ladders.get(&key.item()).unwrap_or(&no_ladder);
        let over = windows
            .get(catalog.id.as_str())
            .expect("every owning catalogue has its windows");
        batch.push(materialise::commodity(
            key, group, ladder, catalog, over, now,
        ));
        if batch.len() >= BATCH {
            markets += read_model.stage(version, &batch).await?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        markets += read_model.stage(version, &batch).await?;
    }

    Ok(RegionReport {
        markets,
        oldest,
        newest,
    })
}

/// Split a region's history into one slice per market.
///
/// The rows arrive ordered by item, so this is a walk rather than a sort or a
/// map. Same property `history_in_region` is written to have.
fn grouped(history: &[PriceSample]) -> impl Iterator<Item = &[PriceSample]> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= history.len() {
            return None;
        }
        let item = history[start].item;
        let mut end = start;
        while end < history.len() && history[end].item == item {
            end += 1;
        }
        let group = &history[start..end];
        start = end;
        Some(group)
    })
}

/// Roll one region's per-realm markets up, region-wide and per realm.
///
/// One read of the region's window rather than one per item or one per realm:
/// the page used to ask for one item's history across 92 realms, and the
/// materialiser asks once for all of them.
#[allow(clippy::too_many_arguments)]
async fn realm_region<E: Ports>(
    env: &E,
    owners: &BTreeMap<ItemId, &Catalog>,
    public: &[&Catalog],
    fallback: &Catalog,
    region: Region,
    version: u64,
    now: Millis,
) -> app_core::error::RepoResult<RegionReport> {
    let Some((from, _)) = REALM_WINDOW.bounds(fallback, now) else {
        return Ok(RegionReport {
            markets: 0,
            oldest: None,
            newest: None,
        });
    };

    let history = env
        .store()
        .realm_prices()
        .window_in_region(region, from)
        .await?;
    let oldest = history.iter().map(|s| s.observed_at).min();
    let newest = history.iter().map(|s| s.observed_at).max();

    // Split by owning catalogue, then roll each part up under its own.
    //
    // Which track a variant belongs to is a catalogue rule, and last season's
    // bonus ids are in last season's catalogue. Rolling the whole region up
    // under the live one filed every archived BoE as gear with an unresolved
    // track -- a market named after nothing, which is precisely what §8's
    // "group on the track bonus" was written to prevent, one rollover later.
    //
    // Partitioning by item is safe because every figure a roll-up derives is
    // per item: the "newest per realm across every track" rule that decides
    // whether a variant is still listed never looks across two items.
    //
    // Skipped entirely while there is one catalogue, which is every deployment
    // until its first rollover. The partition moves half a million samples into
    // fresh vectors, and doing that to arrive at the list we already have would
    // be a rollover's cost charged to everybody who has not had one.
    // Every realm's newest ladder, in one query. `None` everywhere on an
    // archive gathered before Phase 7, which is what makes the depth panel say
    // so rather than draw an empty market.
    let ladders: materialise::RealmLadders = env
        .store()
        .realm_prices()
        .latest_ladders_in_region(region)
        .await?
        .into_iter()
        .map(|(realm, item, variant, ladder)| ((realm, item, variant), ladder))
        .collect();

    let rollups = if public.len() < 2 {
        materialise::rollups(&history, &ladders, fallback, &REALM_WINDOW)
    } else {
        let mut mine: BTreeMap<&str, Vec<app_core::market::RealmSample>> = BTreeMap::new();
        for sample in history {
            let owner = owners
                .get(&sample.item)
                .map(|c| c.id.as_str())
                .unwrap_or(fallback.id.as_str());
            mine.entry(owner).or_default().push(sample);
        }
        let mut rollups = Vec::new();
        for (id, samples) in &mine {
            let catalog = public
                .iter()
                .copied()
                .find(|c| c.id == *id)
                .unwrap_or(fallback);
            rollups.extend(materialise::rollups(
                samples,
                &ladders,
                catalog,
                &REALM_WINDOW,
            ));
        }
        rollups
    };
    let read_model = env.store().read_model();

    let mut markets = 0u64;
    for batch in rollups.chunks(BATCH) {
        markets += read_model.stage_rollups(version, batch).await?;
    }

    Ok(RegionReport {
        markets,
        oldest,
        newest,
    })
}

fn min_option(a: Option<Millis>, b: Option<Millis>) -> Option<Millis> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (some, None) | (None, some) => some,
    }
}

fn max_option(a: Option<Millis>, b: Option<Millis>) -> Option<Millis> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (some, None) | (None, some) => some,
    }
}
