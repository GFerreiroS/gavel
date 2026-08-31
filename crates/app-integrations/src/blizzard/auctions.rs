//! The region-wide commodity auction house.
//!
//! `GET /data/wow/auctions/commodities?namespace=dynamic-{region}`
//!
//! Two things about this endpoint drive the design:
//!
//! * It costs **25** against the hourly request budget rather than 1, and the
//!   data only changes once an hour, so `If-Modified-Since` is not an
//!   optimisation here -- it is the difference between polling politely and
//!   burning the budget.
//! * The payload covers every commodity in the region, which is far more than
//!   we track. Listings are filtered during collection so nothing downstream
//!   ever sees the rest.

use std::collections::BTreeSet;

use app_core::error::{AppError, AppResult};
use app_core::market::{CommodityProvider, Copper, ItemId, Listing, Region, Snapshot};
use cluster_core::{Clock, Millis};
use serde::Deserialize;

use super::token::TokenSource;
use super::{BlizzardConfig, BlizzardCredentials};

pub struct BlizzardAuctions<C> {
    http: reqwest::Client,
    token: TokenSource<C>,
    clock: C,
    metrics: Option<std::sync::Arc<app_core::Metrics>>,
}

impl<C: Clock + Clone + 'static> BlizzardAuctions<C> {
    pub fn new(
        config: BlizzardConfig,
        credentials: BlizzardCredentials,
        clock: C,
    ) -> AppResult<Self> {
        let metrics = config.metrics.clone();
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(config.user_agent.clone())
            .build()
            .map_err(|e| AppError::internal(format!("building HTTP client: {e}")))?;
        Ok(Self {
            token: TokenSource::new(http.clone(), config, credentials, clock.clone()),
            http,
            clock,
            metrics,
        })
    }
}

impl<C: Clock + Clone + 'static> CommodityProvider for BlizzardAuctions<C> {
    fn provider_name(&self) -> &'static str {
        "Blizzard Game Data API"
    }

    async fn commodities(
        &self,
        region: Region,
        wanted: &[ItemId],
        if_modified_since: Option<Millis>,
    ) -> AppResult<Snapshot> {
        let bearer = self.token.bearer().await?;
        let url = format!("{}/data/wow/auctions/commodities", region.api_host());

        let mut request = self
            .http
            .get(&url)
            .bearer_auth(bearer)
            .query(&[("namespace", region.namespace().as_str())]);

        if let Some(since) = if_modified_since {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, http_date(since));
        }

        let response = request
            .send()
            .await
            .map_err(|e| AppError::Integration(format!("commodities request failed: {e}")))?;

        match response.status().as_u16() {
            200 => {}
            304 => return Ok(Snapshot::NotModified),
            401 | 403 => {
                return Err(AppError::Integration(
                    "Battle.net rejected the credentials for the commodities endpoint".into(),
                ));
            }
            429 => {
                return Err(AppError::Integration(
                    "Battle.net rate limit reached; the commodities endpoint costs 25 per call"
                        .into(),
                ));
            }
            status => {
                return Err(AppError::Integration(format!(
                    "commodities endpoint returned HTTP {status}"
                )));
            }
        }

        // The snapshot's own generation time, not ours: samples must land on
        // the hour Blizzard produced them or the history smears.
        let generated_at =
            super::snapshot_time(response.headers(), self.clock.now(), "commodities");

        let payload: CommoditiesResponse = super::bounded_json(
            response,
            128 * 1024 * 1024,
            "commodities",
            self.metrics.as_deref(),
        )
        .await?;
        if payload.auctions.len() > 3_000_000 {
            if let Some(metrics) = &self.metrics {
                metrics.upstream_oversize();
            }
            return Err(AppError::Integration(
                "commodities payload contains too many auctions".into(),
            ));
        }

        let wanted: BTreeSet<u32> = wanted.iter().map(|i| i.get()).collect();
        let total = payload.auctions.len();
        let listings: Vec<Listing> = payload
            .auctions
            .into_iter()
            .filter(|a| wanted.contains(&a.item.id))
            .map(|a| Listing {
                item: ItemId(a.item.id),
                unit_price: Copper(a.unit_price),
                quantity: a.quantity,
            })
            .collect();

        tracing::info!(
            region = %region,
            scanned = total,
            kept = listings.len(),
            "commodity snapshot fetched"
        );

        Ok(Snapshot::Fresh {
            // Falling back to "now" would be wrong for history, but a missing
            // Last-Modified is not worth failing the whole collection over.
            generated_at,
            listings,
        })
    }
}

fn http_date(at: Millis) -> String {
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_millis(at.get()))
}

// --- wire format ---------------------------------------------------------
// Private. Commodities are unit-priced, so `unit_price` is the field that
// matters; `bid`, `buyout` and `time_left` do not apply to them.

#[derive(Debug, Deserialize)]
struct CommoditiesResponse {
    #[serde(default)]
    auctions: Vec<RawAuction>,
}

#[derive(Debug, Deserialize)]
struct RawAuction {
    item: RawItem,
    quantity: u64,
    #[serde(default)]
    unit_price: u64,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    id: u32,
}
