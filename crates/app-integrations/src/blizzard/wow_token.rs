//! WoW Token price: `GET /data/wow/token/index?namespace=dynamic-{region}`.

use app_core::error::{AppError, AppResult};
use app_core::market::Region;
use app_core::repo::WowTokenPrice;
use cluster_core::{Clock, Millis};
use serde::Deserialize;

use super::token::TokenSource;
use super::{BlizzardConfig, BlizzardCredentials};

pub struct BlizzardWowToken<C> {
    http: reqwest::Client,
    token: TokenSource<C>,
    clock: C,
    metrics: Option<std::sync::Arc<app_core::Metrics>>,
}

impl<C: Clock + Clone + 'static> BlizzardWowToken<C> {
    pub fn new(
        config: BlizzardConfig,
        credentials: BlizzardCredentials,
        clock: C,
    ) -> AppResult<Self> {
        let metrics = config.metrics.clone();
        let http = reqwest::Client::builder()
            .timeout(config.item_timeout)
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

    pub async fn price(&self, region: Region) -> AppResult<WowTokenPrice> {
        let bearer = self.token.bearer().await?;
        let response = self
            .http
            .get(format!("{}/data/wow/token/index", region.api_host()))
            .bearer_auth(bearer)
            .query(&[("namespace", region.namespace())])
            .send()
            .await
            .map_err(|e| AppError::Integration(format!("WoW Token request failed: {e}")))?;

        match response.status().as_u16() {
            200 => {}
            401 | 403 => {
                return Err(AppError::Integration(
                    "Battle.net rejected the credentials for the WoW Token endpoint".into(),
                ));
            }
            429 => {
                return Err(AppError::Integration(
                    "Battle.net rate limit reached on the WoW Token endpoint".into(),
                ));
            }
            status => {
                return Err(AppError::Integration(format!(
                    "WoW Token endpoint returned HTTP {status}"
                )));
            }
        }

        let payload: RawToken =
            super::bounded_json(response, 64 * 1024, "WoW Token", self.metrics.as_deref()).await?;
        payload.into_price(region, self.clock.now())
    }
}

#[derive(Debug, Deserialize)]
struct RawToken {
    #[serde(default)]
    price: u64,
    #[serde(default)]
    last_updated_timestamp: u64,
}

impl RawToken {
    fn into_price(self, region: Region, received_at: Millis) -> AppResult<WowTokenPrice> {
        if self.price == 0 {
            return Err(AppError::Integration(
                "WoW Token response did not contain a price".into(),
            ));
        }
        Ok(WowTokenPrice {
            region,
            observed_at: if self.last_updated_timestamp == 0 {
                received_at
            } else {
                Millis(self.last_updated_timestamp)
            },
            price: app_core::market::Copper(self.price),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_payload_uses_its_upstream_timestamp_and_price() {
        let raw: RawToken = serde_json::from_str(
            r#"{"last_updated_timestamp": 1735689600000, "price": 234567000}"#,
        )
        .unwrap();

        assert_eq!(
            raw.into_price(Region::Eu, Millis(1)).unwrap(),
            WowTokenPrice {
                region: Region::Eu,
                observed_at: Millis(1_735_689_600_000),
                price: app_core::market::Copper(234_567_000),
            }
        );
    }

    #[test]
    fn token_payload_rejects_a_missing_price() {
        let raw: RawToken = serde_json::from_str(r#"{}"#).unwrap();
        assert!(raw.into_price(Region::Us, Millis(1)).is_err());
    }
}
