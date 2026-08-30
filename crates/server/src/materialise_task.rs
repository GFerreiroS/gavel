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
use app_core::market::{Catalog, PriceSample, Region};
use app_core::repo::{PriceRepository, ReadModelRepository, RealmPriceRepository, Store};
use cluster_core::Millis;

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
    let Some(catalog) = env.active_catalog() else {
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
            "commodity" => commodity_region(env, catalog, region, version, now).await,
            _ => realm_region(env, catalog, region, version, now).await,
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
    catalog: &Catalog,
    region: Region,
    version: u64,
    now: Millis,
) -> app_core::error::RepoResult<RegionReport> {
    // One query for the region rather than one per item. Ordered by market and
    // then by time, so the grouping below is a walk rather than a sort.
    let history = env.store().prices().history_in_region(region).await?;
    let oldest = history.first().map(|s| s.observed_at);
    let newest = history.iter().map(|s| s.observed_at).max();

    let windows = Window::all_for(catalog);
    let read_model = env.store().read_model();

    let mut markets = 0u64;
    let mut batch: Vec<Materialised> = Vec::with_capacity(BATCH);
    for group in grouped(&history) {
        let key = catalog.market_of(&group[0]);
        batch.push(materialise::commodity(key, group, catalog, &windows, now));
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
/// The rows arrive ordered by item, so this is a walk. Commodity markets are
/// one per item id -- the rank is part of the key but it is derived from the
/// item, not an axis the rows vary along.
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
async fn realm_region<E: Ports>(
    env: &E,
    catalog: &Catalog,
    region: Region,
    version: u64,
    now: Millis,
) -> app_core::error::RepoResult<RegionReport> {
    let Some((from, _)) = REALM_WINDOW.bounds(catalog, now) else {
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

    let rollups = materialise::rollups(&history, catalog, &REALM_WINDOW);
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
