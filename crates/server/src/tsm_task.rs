//! Independent collection of TradeSkillMaster's static CSV feeds.
//!
//! This is deliberately outside the auction collection loop: commodities
//! refresh roughly every three hours and regional sales figures once daily.

use std::time::Duration;

use app_core::Ports;
use app_core::market::TsmContrast;
use app_core::repo::{Store, TsmRepository};
use app_integrations::TsmClient;

const COMMODITY_CADENCE: Duration = Duration::from_secs(3 * 60 * 60);
const REGION_CADENCE: Duration = Duration::from_secs(24 * 60 * 60);

pub fn spawn<E: Ports>(env: E) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match TsmClient::new() {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(%error, "could not build TSM client; TSM collection is disabled");
                return;
            }
        };
        tokio::join!(
            commodities(env.clone(), client.clone()),
            region_items(env, client)
        );
    })
}

async fn commodities<E: Ports>(env: E, client: TsmClient) {
    let mut ticker = tokio::time::interval(COMMODITY_CADENCE);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(catalog) = env.active_catalog() else {
            continue;
        };
        let wanted = catalog.tracked_ids();
        for region in &env.market().regions {
            let samples = match client.commodities(*region, &wanted).await {
                Ok(samples) => samples,
                Err(error) => {
                    tracing::warn!(%region, %error, "TSM commodity collection failed");
                    continue;
                }
            };
            let Some(observed_at) = samples.first().map(|sample| sample.observed_at) else {
                tracing::warn!(%region, "TSM commodity file contained no catalogue items");
                continue;
            };
            match env
                .store()
                .tsm()
                .has_commodity_snapshot(*region, observed_at)
                .await
            {
                Ok(true) => {
                    tracing::debug!(%region, %observed_at, "TSM commodity snapshot unchanged")
                }
                Ok(false) => match env.store().tsm().record_commodity_samples(&samples).await {
                    Ok(rows) => match env.store().tsm().contrast(*region, observed_at).await {
                        Ok(contrasts) => log_contrast(*region, observed_at, rows, &contrasts),
                        Err(error) => tracing::warn!(%region, %error, "TSM contrast test failed"),
                    },
                    Err(error) => {
                        tracing::warn!(%region, %error, "storing TSM commodity data failed")
                    }
                },
                Err(error) => {
                    tracing::warn!(%region, %error, "checking TSM commodity snapshot failed")
                }
            }
        }
    }
}

async fn region_items<E: Ports>(env: E, client: TsmClient) {
    let mut ticker = tokio::time::interval(REGION_CADENCE);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(catalog) = env.active_catalog() else {
            continue;
        };
        let wanted = catalog.tracked_ids();
        for region in &env.market().regions {
            match client.region_items(*region, &wanted).await {
                Ok(samples) => match env.store().tsm().record_region_daily(&samples).await {
                    Ok(rows) => tracing::info!(%region, rows, "collected TSM regional sales data"),
                    Err(error) => {
                        tracing::warn!(%region, %error, "storing TSM regional sales data failed")
                    }
                },
                Err(error) => {
                    tracing::warn!(%region, %error, "TSM regional sales collection failed")
                }
            }
        }
    }
}

fn log_contrast(
    region: app_core::market::Region,
    observed_at: cluster_core::Millis,
    rows: u64,
    contrasts: &[TsmContrast],
) {
    let min_buyout_matches = contrasts
        .iter()
        .filter(|contrast| contrast.min_buyout_matches)
        .count();
    tracing::info!(
        %region,
        %observed_at,
        rows,
        compared = contrasts.len(),
        min_buyout_matches,
        "stored TSM commodity data and ran internal contrast test"
    );
}
