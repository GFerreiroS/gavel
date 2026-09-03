//! Blizzard Game Data API.
//!
//! Two pieces: an OAuth client-credentials token that is fetched once and
//! reused, and the region-wide commodity auction house.
//!
//! Credentials come from the environment and are never logged, never rendered
//! and never persisted.

mod auctions;
mod items;
mod realms;
mod token;
mod wow_token;

use std::time::Duration;

use app_core::error::{AppError, AppResult};
use serde::de::DeserializeOwned;

pub use auctions::BlizzardAuctions;
pub use items::BlizzardItems;
pub use realms::BlizzardRealms;
pub use token::TokenSource;
pub use wow_token::BlizzardWowToken;

/// Client credentials, read from the environment.
#[derive(Clone)]
pub struct BlizzardCredentials {
    pub client_id: String,
    client_secret: String,
}

impl BlizzardCredentials {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// `None` when the variables are absent, which is the normal state until
    /// someone registers a client. The tracker then reports itself as
    /// unconfigured rather than failing at startup.
    pub fn from_env() -> Option<Self> {
        let client_id = std::env::var("BLIZZARD_CLIENT_ID").ok()?;
        let client_secret = std::env::var("BLIZZARD_CLIENT_SECRET").ok()?;
        if client_id.trim().is_empty() || client_secret.trim().is_empty() {
            return None;
        }
        Some(Self::new(client_id, client_secret))
    }

    pub(crate) fn secret(&self) -> &str {
        &self.client_secret
    }
}

/// Hand-written so the secret cannot escape through a stray `{:?}`.
impl std::fmt::Debug for BlizzardCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlizzardCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct BlizzardConfig {
    /// Region-agnostic OAuth endpoint.
    pub oauth_url: String,
    /// The commodities payload is large; this is not a snappy request.
    pub timeout: Duration,
    /// Single-item lookups are small, and one sits behind a hover: a slow
    /// upstream must not hold a browser connection open for two minutes.
    pub item_timeout: Duration,
    pub user_agent: String,
    pub metrics: Option<std::sync::Arc<app_core::Metrics>>,
}

impl Default for BlizzardConfig {
    fn default() -> Self {
        Self {
            oauth_url: "https://oauth.battle.net/token".to_string(),
            timeout: Duration::from_secs(120),
            item_timeout: Duration::from_secs(10),
            user_agent: concat!("wow-auction-tracker/", env!("CARGO_PKG_VERSION")).to_string(),
            metrics: None,
        }
    }
}

pub(crate) async fn bounded_json<T: DeserializeOwned>(
    mut response: reqwest::Response,
    maximum: usize,
    what: &str,
    metrics: Option<&app_core::Metrics>,
) -> AppResult<T> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        if let Some(metrics) = metrics {
            metrics.upstream_oversize();
        }
        return Err(AppError::Integration(format!(
            "{what} response exceeds the {maximum} byte limit"
        )));
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(maximum as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| AppError::Integration(format!("reading {what} response failed: {e}")))?
    {
        if body.len().saturating_add(chunk.len()) > maximum {
            if let Some(metrics) = metrics {
                metrics.upstream_oversize();
            }
            return Err(AppError::Integration(format!(
                "{what} response exceeds the {maximum} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|e| AppError::Integration(format!("unexpected {what} payload: {e}")))
}

pub(crate) fn snapshot_time(
    headers: &reqwest::header::HeaderMap,
    received_at: cluster_core::Millis,
    what: &str,
) -> cluster_core::Millis {
    let parsed = headers
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| cluster_core::Millis(value.as_millis() as u64));
    parsed.unwrap_or_else(|| {
        tracing::warn!(
            endpoint = what,
            "response had no valid Last-Modified header"
        );
        received_at
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_time_uses_a_valid_header_and_reception_time_otherwise() {
        let now = cluster_core::Millis(9_999_000);
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(snapshot_time(&headers, now, "test"), now);
        headers.insert(reqwest::header::LAST_MODIFIED, "invalid".parse().unwrap());
        assert_eq!(snapshot_time(&headers, now, "test"), now);
        headers.insert(
            reqwest::header::LAST_MODIFIED,
            "Thu, 01 Jan 1970 00:00:01 GMT".parse().unwrap(),
        );
        assert_eq!(
            snapshot_time(&headers, now, "test"),
            cluster_core::Millis(1_000)
        );
        headers.insert(
            reqwest::header::LAST_MODIFIED,
            "Thu, 01 Jan 1970 00:00:00 GMT".parse().unwrap(),
        );
        assert_eq!(
            snapshot_time(&headers, now, "test"),
            cluster_core::Millis::ZERO,
            "a legitimate epoch header is distinct from a missing header"
        );
    }

    async fn local_response(raw: &'static [u8]) -> reqwest::Response {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(raw).await.unwrap();
        });
        reqwest::get(format!("http://{address}/")).await.unwrap()
    }

    #[tokio::test]
    async fn bounded_json_rejects_declared_and_chunked_oversize_bodies() {
        let normal = local_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n{\"a\":1}",
        )
        .await;
        let parsed: serde_json::Value = bounded_json(normal, 16, "test", None).await.unwrap();
        assert_eq!(parsed["a"], 1);

        let declared = local_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\nConnection: close\r\n\r\n01234567890123456789012345678901",
        )
        .await;
        assert!(
            bounded_json::<serde_json::Value>(declared, 8, "test", None)
                .await
                .is_err()
        );

        let chunked = local_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n8\r\n12345678\r\n8\r\n12345678\r\n0\r\n\r\n",
        )
        .await;
        assert!(
            bounded_json::<serde_json::Value>(chunked, 12, "test", None)
                .await
                .is_err()
        );

        let truncated = local_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n{\"a\":",
        )
        .await;
        assert!(
            bounded_json::<serde_json::Value>(truncated, 16, "test", None)
                .await
                .is_err()
        );
    }
}
