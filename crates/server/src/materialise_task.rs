//! Recalculating the read model, and publishing it.
//!
//! CLAUDE.md §15: **calculate on update, read on request.** The collector
//! persists observations; this reduces them into the rows a page reads; and a
//! page reads the last complete version with its real timestamp, whatever this
//! is doing at the time.
//!
//! Called by the collector after a snapshot lands. The pure calculation runs
//! on whatever the cluster has -- an in-process worker, a worker on another
//! machine, or this task itself when there is nobody -- and that changes only
//! *where* `market::materialise` runs, never what it produces and never who
//! publishes it. `cargo run` has to keep being the whole story, so the local
//! path is the one that stays and the fallback is not an error case.

use app_core::Ports;
use app_core::market::materialise::{self, ALGORITHM_VERSION, Materialised};
use app_core::market::window::Window;
use app_core::market::{Catalog, ItemId, Region};
use app_core::repo::{PriceRepository, ReadModelRepository, RealmPriceRepository, Store};
use cluster_core::ClusterControl;
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

/// How long the coordinator waits for a distributed rebuild before finishing
/// it itself.
///
/// Generous against the measurement: a full commodity materialisation of the
/// real archive is 0.79 s in one process, so a cluster that has not finished in
/// a minute is a cluster with a problem rather than a slow one. Giving up is
/// not abandoning the version -- the local path completes it, which is what
/// keeps `cargo run` the whole story (§2) and what stops a wedged worker from
/// costing a publication.
const CLUSTER_DEADLINE_MS: u64 = 60_000;

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
pub async fn publish<E: Ports>(
    env: &E,
    artifacts: &std::sync::Arc<crate::analysis_work::Artifacts>,
    commodity: &[Region],
    per_realm: &[Region],
) {
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
            "commodity" => {
                commodity_region(env, artifacts, &owners, fallback, region, version, now).await
            }
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

#[allow(clippy::too_many_arguments)]
async fn commodity_region<E: Ports>(
    env: &E,
    artifacts: &std::sync::Arc<crate::analysis_work::Artifacts>,
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
    let ladders: std::collections::BTreeMap<ItemId, app_core::market::Ladder> = env
        .store()
        .prices()
        .latest_ladders(region)
        .await?
        .into_iter()
        .map(|(item, _, ladder)| (item, ladder))
        .collect();
    // One window list per catalogue rather than per market: they are the same
    // dozen strings for every item a catalogue owns, and building them 2,042
    // times would be §11b's linear-scan-inside-a-loop in another costume.
    let mut windows: BTreeMap<String, Vec<Window>> = BTreeMap::new();
    for catalog in owners.values() {
        windows
            .entry(catalog.id.clone())
            .or_insert_with(|| Window::all_for(catalog));
    }
    windows
        .entry(fallback.id.clone())
        .or_insert_with(|| Window::all_for(fallback));

    // Cut into partitions of a measured size. The same cut whichever way the
    // work is then run, so "the cluster did it" and "this process did it"
    // cannot be two different partitionings producing two different answers.
    let owned: BTreeMap<ItemId, std::sync::Arc<Catalog>> = owners
        .iter()
        .map(|(item, catalog)| (*item, std::sync::Arc::new((*catalog).clone())))
        .collect();
    let spare = std::sync::Arc::new(fallback.clone());
    let inputs = crate::analysis_work::partition(
        region,
        &history,
        &ladders,
        |item| owned.get(&item).cloned().unwrap_or_else(|| spare.clone()),
        |catalog| windows.get(&catalog.id).cloned().unwrap_or_default(),
        now,
    );

    let rows = match distribute(env, artifacts, version, inputs).await {
        Some(rows) => rows,
        // No cluster capacity, or it did not finish in time. The materialiser
        // that has always run here finishes the job: §16 requires local
        // execution to be preserved when there are no remote workers, and this
        // is that requirement with teeth -- the read path never learns which
        // way it went.
        None => local(artifacts, version).await,
    };

    let read_model = env.store().read_model();
    let mut markets = 0u64;
    for batch in rows.chunks(BATCH) {
        markets += read_model.stage(version, batch).await?;
    }

    Ok(RegionReport {
        markets,
        oldest,
        newest,
    })
}

/// Run this version's partitions on the cluster, and return their rows.
///
/// `None` when the cluster cannot or did not do it, which is a signal to
/// finish locally rather than an error: nothing has been staged yet, so
/// falling back costs the work again and nothing else.
async fn distribute<E: Ports>(
    env: &E,
    artifacts: &std::sync::Arc<crate::analysis_work::Artifacts>,
    version: u64,
    inputs: Vec<crate::analysis_work::Input>,
) -> Option<Vec<Materialised>> {
    if inputs.is_empty() {
        return Some(Vec::new());
    }

    // Is there anything to distribute *to*? Asked before submitting, because
    // discovering it afterwards means waiting out the deadline: with
    // `--workers 0` and no remote worker attached, the job sat unassigned and
    // every region paid a full minute before falling back -- four minutes to
    // start a server. §16 asks for local execution to be preserved when there
    // are no remote workers, and a minute of waiting is tolerating that case
    // rather than preserving it.
    let takers = env
        .cluster()
        .nodes()
        .await
        .into_iter()
        .filter(|node| {
            // Alive, and not asked about its roles.
            //
            // Starting counts because a node that has just come up will take
            // the task by the time it is dispatched. The *role* is deliberately
            // not checked: a worker joins with an empty role set and the
            // supervisor assigns Compute on a later tick, so requiring it here
            // sent every rebuild local for the first seconds of a process --
            // three remote workers connected, and the coordinator materialised
            // the whole archive itself while they sat idle. Eligibility is the
            // scheduler's decision and it already makes it; what this asks is
            // only whether there is anybody at all, and the deadline below is
            // what covers a cluster whose nodes never become eligible.
            matches!(
                node.status,
                cluster_core::NodeStatus::Healthy | cluster_core::NodeStatus::Starting
            )
        })
        .count();
    if takers == 0 {
        tracing::debug!("no compute node is available; materialising here");
        artifacts.begin(version, ALGORITHM_VERSION, inputs);
        return None;
    }

    let partitions = artifacts.begin(version, ALGORITHM_VERSION, inputs);

    let job = match env
        .cluster()
        .submit_job(cluster_core::JobSpec::Analysis {
            version,
            algorithm: ALGORITHM_VERSION,
            partitions,
        })
        .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::info!(%error, "no cluster capacity for the analysis; running it here");
            return None;
        }
    };

    let started = std::time::Instant::now();
    loop {
        if let Some(rows) = artifacts.collect(version) {
            tracing::info!(
                %job, version, partitions,
                seconds = started.elapsed().as_secs_f32(),
                "the cluster materialised this version"
            );
            return Some(rows);
        }
        if let Some(detail) = env.cluster().job(job).await
            && detail.job.state.is_terminal()
        {
            // Terminal without every partition back is a failed rebuild, not a
            // partial one. §15's third point: an incomplete candidate stays
            // unreachable, so this returns nothing and the caller redoes it.
            let (done, all) = artifacts.done();
            tracing::warn!(
                %job, version, done, all, state = ?detail.job.state,
                "the cluster finished without every partition; falling back"
            );
            return None;
        }
        if started.elapsed().as_millis() as u64 > CLUSTER_DEADLINE_MS {
            let (done, all) = artifacts.done();
            tracing::warn!(%job, version, done, all, "the cluster did not finish in time");
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Materialise every registered partition in this process.
///
/// The same function a worker runs, called directly. That is what makes the
/// local and distributed paths produce the same rows rather than two
/// implementations that agree today.
async fn local(
    artifacts: &std::sync::Arc<crate::analysis_work::Artifacts>,
    version: u64,
) -> Vec<Materialised> {
    let workload = crate::analysis_work::MarketWorkload::new();
    let (_, all) = artifacts.done();
    let here = cluster_core::NodeId(0);
    let store = artifacts.clone();
    // On the blocking pool: this is the same CPU-bound reduction a worker
    // does, and doing it on the async runtime would stall every heartbeat in
    // the process (§5).
    //
    // Through the same two ports a worker goes through -- fetch the input,
    // return the artifact -- rather than reaching into the store directly.
    // Same code, one transport shorter, which is what makes "local and remote
    // agree" a property of the design instead of a test that keeps passing.
    let handle = tokio::task::spawn_blocking(move || {
        use cluster_core::{ArtifactStore, Workload};
        for partition in 0..all as u32 {
            let spec = cluster_core::TaskSpec::Analysis {
                version,
                algorithm: ALGORITHM_VERSION,
                partition,
            };
            let Some(input) = store.input(spec) else {
                continue;
            };
            if let Some(cluster_core::TaskWork::Produced { artifact, .. }) =
                workload.run(here, spec, &input)
            {
                store.produced(spec, &artifact);
            }
        }
    });
    if let Err(error) = handle.await {
        tracing::warn!(%error, "the local materialiser panicked");
    }
    artifacts.collect(version).unwrap_or_default()
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
