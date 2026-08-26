//! OAuth client-credentials token, fetched once and reused.
//!
//! Tokens last about a day. Requesting one per API call would waste most of
//! the hourly request budget on authentication, so it is cached and refreshed
//! only when it is close to expiring.

use app_core::error::{AppError, AppResult};
use cluster_core::{Clock, Millis};
use serde::Deserialize;
use tokio::sync::RwLock;

use super::{BlizzardConfig, BlizzardCredentials};

/// Refresh this far before actual expiry, so a request in flight cannot be
/// caught by the boundary.
const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    expires_at: Millis,
}

pub struct TokenSource<C> {
    http: reqwest::Client,
    config: BlizzardConfig,
    credentials: BlizzardCredentials,
    clock: C,
    cached: RwLock<Option<CachedToken>>,
}

impl<C: Clock> TokenSource<C> {
    pub fn new(
        http: reqwest::Client,
        config: BlizzardConfig,
        credentials: BlizzardCredentials,
        clock: C,
    ) -> Self {
        Self {
            http,
            config,
            credentials,
            clock,
            cached: RwLock::new(None),
        }
    }

    /// A valid bearer token, fetching a new one only when needed.
    pub async fn bearer(&self) -> AppResult<String> {
        let now = self.clock.now();

        if let Some(token) = self.cached.read().await.as_ref()
            && now.get() + REFRESH_MARGIN_MS < token.expires_at.get()
        {
            return Ok(token.value.clone());
        }

        let mut cached = self.cached.write().await;
        // Another task may have refreshed while we waited for the lock.
        if let Some(token) = cached.as_ref()
            && now.get() + REFRESH_MARGIN_MS < token.expires_at.get()
        {
            return Ok(token.value.clone());
        }

        let fresh = self.fetch(now).await?;
        let value = fresh.value.clone();
        *cached = Some(fresh);
        Ok(value)
    }

    async fn fetch(&self, now: Millis) -> AppResult<CachedToken> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        let response = self
            .http
            .post(&self.config.oauth_url)
            .basic_auth(&self.credentials.client_id, Some(self.credentials.secret()))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .map_err(|e| AppError::Integration(format!("Battle.net token request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            // Deliberately does not echo the response body: a 401 here means
            // the credentials are wrong, and the body can quote them back.
            return Err(AppError::Integration(format!(
                "Battle.net token request returned HTTP {}",
                status.as_u16()
            )));
        }

        let token: TokenResponse = response
            .json()
            .await
            .map_err(|e| AppError::Integration(format!("unexpected token payload: {e}")))?;

        tracing::debug!(
            expires_in_s = token.expires_in,
            "obtained Battle.net access token"
        );

        Ok(CachedToken {
            value: token.access_token,
            expires_at: now.plus_ms(token.expires_in.saturating_mul(1000)),
        })
    }
}
