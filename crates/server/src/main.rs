//! Composition root.
//!
//! Build the adapters, hand them to the runtime and the router, serve, and
//! shut down cleanly.

mod analysis_work;
mod collector_task;
mod config;
mod env_file;
mod market;
mod materialise_task;
mod query_timing;
mod runtime;
mod worker;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use app_core::Metrics;
use app_core::auth::{
    Argon2Hasher, OsTokens, PasswordHasher, validate_password, validate_username,
};
use app_core::market::{CatalogSet, ReleaseStates};
use app_core::repo::{
    CacheStore, KeyValueStore, MarketEventRepository, ReleaseRepository, SessionRepository, Store,
    UserRepository,
};
use app_integrations::{
    BlizzardAuctions, BlizzardConfig, BlizzardCredentials, BlizzardItems, DiscordWebhook,
    RaiderIoClient, RaiderIoConfig,
};
use clap::Parser;
use cluster_core::{Clock, EventLog, JobStore, Millis};
use cluster_local::{LocalCluster, SystemClock};
use storage::{SqliteConfig, SqliteStore};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer as _};

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
    let mut cli = Cli::parse();
    init_tracing(&cli.log, cli.server_timing)?;
    cli.resolve_cluster_token()?;
    cli.resolve_bootstrap_admin()?;

    // Worker mode short-circuits everything below: no HTTP, no database, no
    // application state. It dials the coordinator and does what it is told.
    if let Some(address) = cli.connect.clone() {
        return worker::run(&address, cli.cluster_token.clone()).await;
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
    if !cli.host.is_loopback() && !cli.secure_cookies {
        tracing::warn!(
            host = %cli.host,
            "HTTP is bound beyond loopback while APP_SECURE_COOKIES is false; use this only for explicit plain-HTTP development"
        );
    }

    // --- adapters ---------------------------------------------------------
    let store = SqliteStore::connect(&SqliteConfig::new(&cli.database))
        .await
        .context("opening the database")?;

    let clock = SystemClock;
    let metrics = std::sync::Arc::new(Metrics::new());
    let removed = housekeeping(&store, &clock, cli.operation_retention_days).await;
    if removed > 0 {
        tracing::info!(rows = removed, "purged expired sessions and cache entries");
    }
    record_boot_configuration(&store, &cli).await;

    if let Some((username, password)) = cli.bootstrap_admin.take() {
        bootstrap_admin(&store, &clock, username, password).await?;
    }

    // One store handle: the runtime persists jobs, events and role assignments.
    // The inputs and results of whatever analysis version is being built.
    //
    // Both sides of the seam are installed here, because this is the one place
    // that knows the cluster and the market at once (§3). The *store* is the
    // coordinator's: it hands out a partition's input and takes back its rows,
    // whether the worker that ran it was a task in this process or a socket.
    // The *workload* is every worker's, and it is stateless -- which is what
    // lets the same one be installed in a `--connect` process that has no
    // database, no catalogue and no candidate of its own.
    let artifacts = std::sync::Arc::new(analysis_work::Artifacts::new());

    let characters = RaiderIoClient::new(RaiderIoConfig::default(), clock)
        .map_err(|e| anyhow::anyhow!("building the Raider.IO client: {e}"))?;

    // Both of these are optional: the app runs without them, it just cannot
    // collect prices or push alerts.
    let (commodities, realm_auctions, items) = match BlizzardCredentials::from_env() {
        Some(credentials) => {
            tracing::info!("Battle.net credentials loaded");
            // Two adapters, one set of credentials: prices come from the
            // auction house, tooltips from the static game data.
            let blizzard_config = BlizzardConfig {
                metrics: Some(metrics.clone()),
                ..BlizzardConfig::default()
            };
            let auctions =
                BlizzardAuctions::new(blizzard_config.clone(), credentials.clone(), clock)
                    .map_err(|e| anyhow::anyhow!("building the Blizzard client: {e}"))?;
            let realms = app_integrations::BlizzardRealms::new(
                blizzard_config.clone(),
                credentials.clone(),
                clock,
            )
            .map_err(|e| anyhow::anyhow!("building the Blizzard realm client: {e}"))?;
            let items = BlizzardItems::new(blizzard_config, credentials, clock)
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

    // Seed the lifecycle for catalogues this database has never seen, then
    // read back what it actually holds. `seed` never overwrites: a state an
    // administrator set outranks the one this binary shipped with, or an
    // upgrade would silently undo an activation.
    let releases = ReleaseStates::new();
    let seeded = store
        .releases()
        .seed(&catalogs.shipped_states(), clock.now())
        .await
        .context("seeding catalogue release states")?;
    let known = store
        .releases()
        .releases()
        .await
        .context("reading catalogue release states")?;
    tracing::info!(
        seeded,
        known = known.len(),
        active = known
            .iter()
            .find(|r| r.state.is_active())
            .map(|r| r.catalog.as_str())
            .unwrap_or("none"),
        "catalogue releases"
    );
    releases.replace(known.into_iter().map(|r| (r.catalog, r.state)));

    // The timeline every catalogue already states: when each patch shipped and
    // when each raid opened. Nothing is inferred -- these are dates a reviewer
    // read in `catalogs.json` -- and recording them is idempotent by id, so
    // starting twice does not produce two copies of a patch release.
    //
    // Archived catalogues too, not only the active one: the archive is the
    // product, and a frozen expansion whose timeline went missing is an
    // archive that cannot explain itself.
    let timeline: Vec<_> = catalogs
        .catalogs
        .iter()
        .flat_map(app_core::market::event::from_catalogue)
        .collect();
    match store.market_events().record(&timeline).await {
        Ok(added) => tracing::info!(added, known = timeline.len(), "catalogue timeline"),
        // Not fatal. The timeline is what Phase 8 correlates against; a page
        // that shows prices without it is still the product.
        Err(error) => tracing::warn!(%error, "could not record the catalogue timeline"),
    }

    // Only now start concurrent writers. Bootstrap, release seeding and the
    // catalogue timeline above are startup transactions and must not race a
    // supervisor write, which used to produce SQLITE_BUSY_SNAPSHOT (517).
    let mut cluster_config = cli.cluster_config();
    cluster_config.workload = Some(std::sync::Arc::new(analysis_work::MarketWorkload::new()));
    cluster_config.artifacts = Some(artifacts.clone());
    let (cluster, supervisor) = LocalCluster::start(cluster_config, store.cluster_handle());
    tracing::info!(workers = cli.workers, "worker pool starting");

    let market_config = market::config(
        cli.regions(),
        cli.realms(),
        cli.market_interval_minutes,
        cli.market_retain_days,
        cli.market_downsample_days,
        cli.market_ladder_days,
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
        releases,
        market: market_config,
        hasher: std::sync::Arc::new(Argon2Hasher::new()),
        tokens: OsTokens,
        clock,
        config: cli.web_config(),
        metrics,
    });

    // Whether this process expects workers to dial in, which is the one thing
    // the boot rebuild cannot work out for itself: a cluster with no nodes yet
    // and a cluster with no nodes coming look identical from inside.
    let collector =
        collector_task::spawn(env.clone(), artifacts.clone(), cli.worker_listen.is_some());

    // --- serve ------------------------------------------------------------
    // Told to the handlers that hold a connection open -- the SSE stream --
    // so they let go when the process is stopping.
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let app = app_web::router(env.clone(), app_web::Shutdown::new(stopping));
    let address = std::net::SocketAddr::new(cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;

    tracing::info!(%address, "listening on http://{address}");
    if cli.debug_controls {
        tracing::warn!("failure-simulation routes are mounted under /debug");
    }

    // Draining is started by hand rather than by handing `axum` the signal
    // directly, because there are two things to do when the signal arrives and
    // the order matters: first tell the live connections to let go, *then*
    // start waiting for them.
    let (drain, draining) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = draining.await;
        })
        .await
    });

    shutdown_signal().await;
    tracing::info!("shutting down");
    let _ = stop.send(true);
    let _ = drain.send(());

    // A deadline, because "graceful" must not mean "never". Graceful shutdown
    // waits for the responses already in flight; the SSE stream is one that
    // never ends on its own, and one open browser tab used to make Ctrl+C hang
    // for ever -- the only way out was closing the terminal. The signal above
    // is what fixes that properly; this is the backstop, so that whatever gets
    // added next cannot bring the hang back.
    match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
        Ok(Ok(result)) => result.context("serving HTTP")?,
        Ok(Err(e)) => tracing::warn!(error = %e, "the HTTP server task ended badly"),
        Err(_) => {
            tracing::warn!(
                seconds = SHUTDOWN_GRACE.as_secs(),
                "connections still open after the grace period; closing anyway"
            );
            // Aborted, not merely abandoned. Letting the deadline drop the
            // handle detaches the task, and a detached server task still holds
            // its clone of the port bundle -- so `drop(env)` below would not be
            // dropping the last one, the supervisor would never be told to
            // stop, and the wait for it would hang exactly where the connection
            // used to. The deadline has to actually end the thing it gave up
            // waiting for.
            server.abort();
            let _ = (&mut server).await;
        }
    }

    // Dropping the last handle stops the supervisor, which stops the nodes.
    collector.abort();
    let _ = collector.await;
    drop(env);
    // Bounded for the same reason as the server above: a shutdown path that
    // can wait for ever is a shutdown path that eventually does.
    let mut supervisor = supervisor;
    match tokio::time::timeout(SHUTDOWN_GRACE, &mut supervisor).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "cluster lifecycle task ended badly"),
        Err(_) => {
            tracing::warn!(
                seconds = SHUTDOWN_GRACE.as_secs(),
                "the cluster did not stop in time; aborting the lifecycle task"
            );
            supervisor.abort();
            let _ = supervisor.await;
        }
    }
    Ok(())
}

fn init_tracing(filter: &str, server_timing: bool) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(filter).context("parsing the log filter")?;
    // The `--log` filter belongs to the console layer rather than to the
    // registry: a filter added to the registry is global, and a global one at
    // `info` would keep SQLx's statement events from ever being built, so the
    // layer below would count nothing.
    let console = tracing_subscriber::fmt::layer()
        .with_target(true)
        .compact()
        .with_filter(filter);

    // `--server-timing` adds a second layer that reads SQLx's statement events
    // and charges them to the request being served. It carries its own filter,
    // so the console still shows only what `--log` asked for: measurement must
    // not turn into a wall of statements.
    let queries = server_timing.then(query_timing::layer);

    tracing_subscriber::registry()
        .with(console)
        .with(queries)
        .init();
    Ok(())
}

/// Drop rows nobody will ever read again. Cheap, and keeps the database from
/// growing on expired sessions and cache entries alone.
async fn housekeeping(store: &SqliteStore, clock: &SystemClock, operation_days: u64) -> u64 {
    let now = clock.now();
    let sessions = match store.sessions().purge_expired(now).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not purge expired sessions");
            0
        }
    };
    let cache = match store.cache().purge_expired(now).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not purge expired cache entries");
            0
        }
    };
    // The price tables grow on every collection cycle. A plan chosen against
    // last month's statistics is a plan chosen against a different table, and
    // on this archive that was a four-fold difference on every category page.
    store.optimize().await;
    let mut operational = 0;
    if operation_days != 0 {
        let cutoff = Millis(
            now.get()
                .saturating_sub(operation_days.saturating_mul(24 * 60 * 60 * 1000)),
        );
        match store.jobs().prune_terminal_before(cutoff).await {
            Ok(rows) => operational += rows,
            Err(error) => tracing::warn!(%error, "could not prune terminal jobs"),
        }
        match store.events().prune_before(cutoff).await {
            Ok(rows) => operational += rows,
            Err(error) => tracing::warn!(%error, "could not prune cluster events"),
        }
    }
    sessions + cache + operational
}

async fn bootstrap_admin(
    store: &SqliteStore,
    clock: &SystemClock,
    username: String,
    password: String,
) -> anyhow::Result<()> {
    validate_username(&username).context("validating bootstrap administrator username")?;
    validate_password(&password).context("validating bootstrap administrator password")?;
    let hash = tokio::task::spawn_blocking(move || Argon2Hasher::new().hash(&password))
        .await
        .context("joining bootstrap password hash task")??;
    match store
        .users()
        .bootstrap_admin(&username, &hash, clock.now())
        .await
        .context("creating bootstrap administrator")?
    {
        Some(user) => tracing::info!(user = %user.username, "administrator bootstrapped"),
        None => tracing::info!("administrator already exists; bootstrap credentials were not used"),
    }
    Ok(())
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

/// How long to wait for open connections once shutdown has begun.
///
/// Generous for a request that is genuinely mid-flight, and short enough that
/// nobody reaches for the terminal's close button.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Ctrl+C, or the signal a container runtime sends.
///
/// `SIGTERM` matters as much as the keyboard: it is what `docker compose down`
/// and systemd send, and a process that ignores it is a process they wait ten
/// seconds for and then kill.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            // Nothing to listen on is not a reason to refuse to serve; the
            // keyboard still works.
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => tracing::debug!("interrupted"),
        _ = terminate => tracing::debug!("terminated"),
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    #[tokio::test]
    async fn configured_bootstrap_creates_an_admin_and_rejects_weak_credentials() {
        let store = SqliteStore::connect(&SqliteConfig::in_memory())
            .await
            .unwrap();
        let clock = SystemClock;

        assert!(
            bootstrap_admin(&store, &clock, "operator".into(), "short".into())
                .await
                .is_err(),
            "invalid bootstrap credentials are rejected before storage"
        );
        assert!(
            store
                .users()
                .by_username("operator")
                .await
                .unwrap()
                .is_none()
        );

        bootstrap_admin(
            &store,
            &clock,
            "operator".into(),
            "a sufficiently long bootstrap password".into(),
        )
        .await
        .unwrap();
        assert!(
            store
                .users()
                .by_username("operator")
                .await
                .unwrap()
                .unwrap()
                .user
                .is_admin
        );
    }
}
