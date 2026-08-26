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
use app_core::market::{Collector, Outcome};
use app_core::repo::{PriceRepository, Store};
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

    loop {
        ticker.tick().await;
        collect_once(&env).await;
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
