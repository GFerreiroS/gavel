//! Blizzard Game Data API.
//!
//! Two pieces: an OAuth client-credentials token that is fetched once and
//! reused, and the region-wide commodity auction house.
//!
//! Credentials come from the environment and are never logged, never rendered
//! and never persisted (CLAUDE.md 9/30).

mod auctions;
mod token;

use std::time::Duration;

pub use auctions::BlizzardAuctions;
pub use token::TokenSource;

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
    pub user_agent: String,
}

impl Default for BlizzardConfig {
    fn default() -> Self {
        Self {
            oauth_url: "https://oauth.battle.net/token".to_string(),
            timeout: Duration::from_secs(120),
            user_agent: concat!("esp-web-cluster/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}
