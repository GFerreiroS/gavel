//! Composition root.
//!
//! Build the adapters, hand them to the runtime and the router, serve, and
//! shut down cleanly.

mod config;
mod runtime;

use anyhow::Context;
use app_core::auth::{Argon2Hasher, OsTokens};
use app_core::repo::{CacheStore, SessionRepository, Store};
use app_integrations::{RaiderIoClient, RaiderIoConfig};
use clap::Parser;
use cluster_core::Clock;
use cluster_local::{LocalCluster, SystemClock};
use storage::{SqliteConfig, SqliteStore};
use tracing_subscriber::EnvFilter;

use crate::config::Cli;
use crate::runtime::{Inner, Runtime};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log)?;

    // --- adapters ---------------------------------------------------------
    let store = SqliteStore::connect(&SqliteConfig::new(&cli.database))
        .await
        .context("opening the database")?;

    let clock = SystemClock;
    let removed = housekeeping(&store, &clock).await;
    if removed > 0 {
        tracing::info!(rows = removed, "purged expired sessions and cache entries");
    }

    let (cluster, supervisor) = LocalCluster::start(
        cli.cluster_config(),
        store.jobs_handle(),
        store.events_handle(),
    );
    tracing::info!(nodes = cli.nodes, "simulated cluster starting");

    let characters = RaiderIoClient::new(RaiderIoConfig::default(), clock)
        .map_err(|e| anyhow::anyhow!("building the Raider.IO client: {e}"))?;

    let env = Runtime::new(Inner {
        store,
        cluster,
        characters,
        hasher: Argon2Hasher::new(),
        tokens: OsTokens,
        clock,
        config: cli.web_config(),
    });

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

/// Drop rows nobody will ever read again. Cheap, and keeps the file small --
/// which will matter a great deal more on flash than it does here.
async fn housekeeping(store: &SqliteStore, clock: &SystemClock) -> u64 {
    let now = clock.now();
    let sessions = store.sessions().purge_expired(now).await.unwrap_or(0);
    let cache = store.cache().purge_expired(now).await.unwrap_or(0);
    sessions + cache
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
