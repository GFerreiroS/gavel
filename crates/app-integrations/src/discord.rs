//! Discord webhook notifications.
//!
//! A webhook URL is a credential -- anyone holding it can post to the channel
//! -- so it comes from the environment and is never logged.

use app_core::market::{Alert, AlertSeverity, AlertSink};

/// Posts alerts to a Discord channel.
pub struct DiscordWebhook {
    http: reqwest::Client,
    url: String,
}

impl DiscordWebhook {
    pub fn new(url: impl Into<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("building HTTP client: {e}"))?;
        Ok(Self {
            http,
            url: url.into(),
        })
    }

    /// `None` when unset, which is the normal state until someone configures a
    /// channel. Alerts then live in the UI only.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("DISCORD_WEBHOOK_URL").ok()?;
        if url.trim().is_empty() {
            return None;
        }
        Self::new(url).ok()
    }
}

impl std::fmt::Debug for DiscordWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DiscordWebhook { url: <redacted> }")
    }
}

impl AlertSink for DiscordWebhook {
    /// A failed notification must never fail a collection: the alert is
    /// already stored and visible in the UI, so this is best-effort.
    async fn publish(&self, alert: &Alert, item_name: &str) {
        let marker = match alert.severity {
            AlertSeverity::VeryLow => "**VERY LOW**",
            AlertSeverity::Low => "Low",
        };
        let content = format!(
            "{marker} · **{item_name}** on {} — **{}** ({}% below the usual {}), {} in stock",
            alert.region.to_string().to_uppercase(),
            alert.current,
            alert.discount_percent,
            alert.baseline,
            alert.quantity,
        );

        let result = self
            .http
            .post(&self.url)
            // `allowed_mentions` empty: a price alert must never ping a role.
            .json(&serde_json::json!({
                "content": content,
                "allowed_mentions": { "parse": [] }
            }))
            .send()
            .await;

        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => tracing::warn!(
                status = response.status().as_u16(),
                "Discord rejected the alert"
            ),
            Err(e) => tracing::warn!(error = %e, "could not reach Discord"),
        }
    }
}
