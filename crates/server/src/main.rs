//! Composition root.
//!
//! Build the adapters, hand them to the runtime and the router, serve, and
//! shut down cleanly.

mod collector_task;
mod config;
mod env_file;
mod market;
mod runtime;
mod worker;

use std::path::PathBuf;

use anyhow::Context;
use app_core::Metrics;
use app_core::auth::{Argon2Hasher, OsTokens};
use app_core::market::CatalogSet;
use app_core::repo::{CacheStore, KeyValueStore, SessionRepository, Store};
use app_integrations::{
    BlizzardAuctions, BlizzardConfig, BlizzardCredentials, BlizzardItems, DiscordWebhook,
    RaiderIoClient, RaiderIoConfig,
};
use clap::Parser;
use cluster_core::Clock;
use cluster_local::{LocalCluster, SystemClock};
use storage::{SqliteConfig, SqliteStore};
use tracing_subscriber::EnvFilter;

use crate::config::Cli;
use crate::runtime::{Inner, Runtime};

/// Synchronous, and deliberately so.
///
/// `.env` is loaded before the Tokio runtime exists because `set_var` is not
/// thread-safe; doing it inside `#[tokio::main]` would already be racing the
/// runtime's threads. Loading here also means a `.env` can set the `APP_*`
/// flags, since it lands before `Cli::parse`.
fn main() -> anyhow::Result<()> {
    let (env_path, env_keys) = env_file::load_default();
    run(env_path, env_keys)
}

#[tokio::main]
async fn run(env_path: Option<PathBuf>, env_keys: Vec<String>) -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log)?;

    // Worker mode short-circuits everything below: no HTTP, no database, no
    // application state. It dials the coordinator and does what it is told.
    if let Some(address) = cli.connect.clone() {
        return worker::run(&address).await;
    }

    // Key names only -- never values.
    match (&env_path, env_keys.is_empty()) {
        (Some(path), false) => {
            tracing::info!(file = %path.display(), keys = ?env_keys, "loaded environment file")
        }
        (Some(path), true) => tracing::debug!(
            file = %path.display(),
            "environment file found but every key was already set"
        ),
        (None, _) => tracing::debug!("no .env file found"),
    }

    // --- adapters ---------------------------------------------------------
    let store = SqliteStore::connect(&SqliteConfig::new(&cli.database))
        .await
        .context("opening the database")?;

    let clock = SystemClock;
    let removed = housekeeping(&store, &clock).await;
    if removed > 0 {
        tracing::info!(rows = removed, "purged expired sessions and cache entries");
    }
    record_boot_configuration(&store, &cli).await;

    // One store handle: the runtime persists jobs, events and role assignments.
    let (cluster, supervisor) = LocalCluster::start(cli.cluster_config(), store.cluster_handle());
    tracing::info!(workers = cli.workers, "worker pool starting");

    let characters = RaiderIoClient::new(RaiderIoConfig::default(), clock)
        .map_err(|e| anyhow::anyhow!("building the Raider.IO client: {e}"))?;

    // Both of these are optional: the app runs without them, it just cannot
    // collect prices or push alerts.
    let (commodities, realm_auctions, items) = match BlizzardCredentials::from_env() {
        Some(credentials) => {
            tracing::info!("Battle.net credentials loaded");
            // Two adapters, one set of credentials: prices come from the
            // auction house, tooltips from the static game data.
            let auctions =
                BlizzardAuctions::new(BlizzardConfig::default(), credentials.clone(), clock)
                    .map_err(|e| anyhow::anyhow!("building the Blizzard client: {e}"))?;
            let realms = app_integrations::BlizzardRealms::new(
                BlizzardConfig::default(),
                credentials.clone(),
                clock,
            )
            .map_err(|e| anyhow::anyhow!("building the Blizzard realm client: {e}"))?;
            let items = BlizzardItems::new(BlizzardConfig::default(), credentials, clock)
                .map_err(|e| anyhow::anyhow!("building the Blizzard item client: {e}"))?;
            (
                market::Commodities::Live(Box::new(auctions)),
                market::RealmAuctions::Live(Box::new(realms)),
                market::Items::Live(Box::new(items)),
            )
        }
        None => {
            // Point at the file if there is one: "not set" is confusing when
            // the values are visibly sitting in a .env.
            match &env_path {
                Some(path) => tracing::warn!(
                    file = %path.display(),
                    "BLIZZARD_CLIENT_ID / BLIZZARD_CLIENT_SECRET are missing or empty in this \
                     file: price collection and item tooltips are disabled"
                ),
                None => tracing::warn!(
                    "BLIZZARD_CLIENT_ID / BLIZZARD_CLIENT_SECRET not set and no .env found: \
                     price collection and item tooltips are disabled"
                ),
            }
            (
                market::Commodities::Unconfigured,
                market::RealmAuctions::Unconfigured,
                market::Items::Unconfigured,
            )
        }
    };

    let alerts = match DiscordWebhook::from_env() {
        Some(hook) => {
            tracing::info!("Discord webhook configured for price alerts");
            market::Alerts::Discord(Box::new(hook))
        }
        None => {
            tracing::info!("DISCORD_WEBHOOK_URL not set: alerts will appear in the UI only");
            market::Alerts::None
        }
    };

    let catalogs = CatalogSet::embedded();
    let market_config = market::config(
        cli.regions(),
        cli.realms(),
        cli.market_interval_minutes,
        cli.market_retain_days,
        cli.market_downsample_days,
    );

    let env = Runtime::new(Inner {
        store,
        cluster,
        characters,
        commodities,
        realm_auctions,
        items,
        alerts,
        catalogs,
        market: market_config,
        hasher: Argon2Hasher::new(),
        tokens: OsTokens,
        clock,
        config: cli.web_config(),
        metrics: Metrics::new(),
    });

    let collector = collector_task::spawn(env.clone());

    // --- serve ------------------------------------------------------------
    let app = app_web::router(env.clone());
    let address = std::net::SocketAddr::new(cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;

    tracing::info!(%address, "listening on http://{address}");
    if cli.debug_controls {
        tracing::warn!("failure-simulation routes are mounted under /debug");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP")?;

    // Dropping the last handle stops the supervisor, which stops the nodes.
    tracing::info!("shutting down");
    collector.abort();
    drop(env);
    let _ = supervisor.await;
    Ok(())
}

fn init_tracing(filter: &str) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter).context("parsing the log filter")?)
        .with_target(true)
        .compact()
        .init();
    Ok(())
}

/// Drop rows nobody will ever read again. Cheap, and keeps the database from
/// growing on expired sessions and cache entries alone.
async fn housekeeping(store: &SqliteStore, clock: &SystemClock) -> u64 {
    let now = clock.now();
    let sessions = store.sessions().purge_expired(now).await.unwrap_or(0);
    let cache = store.cache().purge_expired(now).await.unwrap_or(0);
    sessions + cache
}

/// Persist what this run was actually configured with, through the generic
/// key/value port.
///
/// Not decoration: when a cluster misbehaves the first question is always
/// "what was it started with", and by then the flags are long gone.
async fn record_boot_configuration(store: &SqliteStore, cli: &Cli) {
    let settings = cli.effective_settings_json();
    if let Err(e) = store
        .kv()
        .put("cluster/boot-config", settings.as_bytes())
        .await
    {
        tracing::warn!(error = %e, "could not record the boot configuration");
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
