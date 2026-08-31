//! Discord webhook notifications.
//!
//! A webhook URL is a credential -- anyone holding it can post to the channel
//! -- so it is never logged, whether it comes from the environment (the
//! instance-wide channel, [`DiscordWebhook`]) or from a person's own account
//! (a per-user channel, [`PerUserDiscord`]).

use app_core::market::{Alert, AlertSeverity, AlertSink};
use app_core::model::UserId;
use app_core::repo::{UserRepository, WatchRepository};

fn format_content(alert: &Alert, item_name: &str) -> String {
    let marker = match alert.severity {
        AlertSeverity::VeryLow => "**VERY LOW**",
        AlertSeverity::Low => "Low",
    };
    format!(
        "{marker} · **{item_name}** on {} — **{}** ({}% below the usual {}), {} in stock",
        alert.region.to_string().to_uppercase(),
        alert.current,
        alert.discount_percent,
        alert.baseline,
        alert.quantity,
    )
}

/// The one HTTP call both `DiscordWebhook` and `PerUserDiscord` make. A
/// failed notification must never fail a collection: the alert is already
/// stored and visible in the UI, so this is best-effort and never returns an
/// error to make one.
async fn post(http: &reqwest::Client, url: &str, alert: &Alert, item_name: &str) {
    let result = http
        .post(url)
        // `allowed_mentions` empty: a price alert must never ping a role.
        .json(&serde_json::json!({
            "content": format_content(alert, item_name),
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

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("building HTTP client: {e}"))
}

/// Posts alerts to one instance-wide Discord channel, configured by an
/// operator via `DISCORD_WEBHOOK_URL`. Every alert this instance raises goes
/// here, regardless of who (if anyone) is watching the market -- an ops
/// channel, not a personal one.
pub struct DiscordWebhook {
    http: reqwest::Client,
    url: String,
}

impl DiscordWebhook {
    pub fn new(url: impl Into<String>) -> Result<Self, String> {
        Ok(Self {
            http: http_client()?,
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
    async fn publish(&self, alert: &Alert, item_name: &str) {
        post(&self.http, &self.url, alert, item_name).await;
    }
}

/// Posts alerts to whoever configured their own webhook and is watching the
/// market the alert is about -- self-service notifications, per
/// `(person, item, region)`, alongside (never instead of) the instance-wide
/// channel above.
pub struct PerUserDiscord<W, U> {
    http: reqwest::Client,
    watches: W,
    users: U,
}

impl<W, U> PerUserDiscord<W, U>
where
    W: WatchRepository,
    U: UserRepository,
{
    pub fn new(watches: W, users: U) -> Result<Self, String> {
        Ok(Self {
            http: http_client()?,
            watches,
            users,
        })
    }

    async fn notify_one(&self, user: UserId, alert: &Alert, item_name: &str) {
        match self.users.discord_webhook(user).await {
            Ok(Some(url)) => post(&self.http, &url, alert, item_name).await,
            Ok(None) => {}
            Err(e) => tracing::warn!(user_id = user, error = %e, "reading a watcher's webhook"),
        }
    }
}

impl<W, U> AlertSink for PerUserDiscord<W, U>
where
    W: WatchRepository,
    U: UserRepository,
{
    async fn publish(&self, alert: &Alert, item_name: &str) {
        let watchers = match self.watches.watchers(alert.item, alert.region).await {
            Ok(watchers) => watchers,
            Err(e) => {
                tracing::warn!(error = %e, "looking up who watches this market");
                return;
            }
        };
        for user in watchers {
            self.notify_one(user, alert, item_name).await;
        }
    }
}
