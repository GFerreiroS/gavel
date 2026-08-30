//! SQLite implementation of the storage ports.

mod cache;
mod cluster;
mod events;
mod jobs;
mod kv;
mod prices;
mod realm_prices;
mod releases;
mod sessions;
mod settings;
mod users;
mod watches;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use app_core::error::{RepoError, RepoResult};
use app_core::repo::Store;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Pool, Sqlite};

pub use cache::SqliteCache;
pub use cluster::SqliteClusterStore;
pub use events::SqliteEvents;
pub use jobs::SqliteJobs;
pub use kv::SqliteKv;
pub use prices::SqlitePrices;
pub use realm_prices::SqliteRealmPrices;
pub use releases::SqliteReleases;
pub use sessions::SqliteSessions;
pub use settings::SqliteSettings;
pub use users::SqliteUsers;
use watches::SqliteWatches;

/// Translate a backend error without letting SQLx types escape the crate.
pub(crate) fn map_err(err: sqlx::Error) -> RepoError {
    match &err {
        sqlx::Error::RowNotFound => RepoError::NotFound,
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            RepoError::Conflict(db.message().to_string())
        }
        _ => RepoError::Backend(err.to_string()),
    }
}

pub(crate) fn corrupt(what: &str, value: impl std::fmt::Display) -> RepoError {
    RepoError::Corrupt(format!("{what}: {value}"))
}

#[derive(Debug, Clone)]
pub struct SqliteConfig {
    pub path: PathBuf,
    /// SQLite writes are serialised anyway; a small pool is plenty and keeps
    /// the connection footprint explicit.
    pub max_connections: u32,
    pub busy_timeout_ms: u64,
    /// How long a pooled connection may live before being recycled. `None`
    /// leaves sqlx's default (30 minutes).
    ///
    /// Exposed because recycling is what exposed the in-memory lifetime bug,
    /// and a bug that takes half an hour to appear needs a way to be
    /// reproduced in a test that takes a second.
    pub max_lifetime_ms: Option<u64>,
}

impl SqliteConfig {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            // Readers, mostly. A single page fans out one read per collected
            // region and WAL lets those run at once; four would make the
            // second visitor queue behind the first. Writes still serialise on
            // SQLite's one writer, which is what `busy_timeout` is for.
            max_connections: 8,
            busy_timeout_ms: 5_000,
            max_lifetime_ms: None,
        }
    }

    /// Throwaway in-memory database.
    ///
    /// Each call gets its own name so that two of them -- two tests running in
    /// parallel, say -- never see each other's rows.
    pub fn in_memory() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self::new(format!(":memory:{n}"))
    }

    fn is_memory(&self) -> bool {
        self.path
            .to_str()
            .is_some_and(|p| p == ":memory:" || p.starts_with(":memory:"))
    }

    fn memory_name(&self) -> String {
        self.path
            .to_str()
            .and_then(|p| p.strip_prefix(":memory:"))
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
            .to_string()
    }
}

/// Owns the connection pool and hands out the repositories.
pub struct SqliteStore {
    pool: Pool<Sqlite>,
    users: SqliteUsers,
    sessions: SqliteSessions,
    jobs: SqliteJobs,
    events: SqliteEvents,
    cache: SqliteCache,
    kv: SqliteKv,
    prices: SqlitePrices,
    realm_prices: SqliteRealmPrices,
    settings: SqliteSettings,
    watches: SqliteWatches,
    releases: SqliteReleases,
}

impl SqliteStore {
    /// The connection pool, for tests that need to hold a hand-written query
    /// against what a port returns.
    ///
    /// Not for application code: domain code goes through the ports and must
    /// never see an SQLx type (CLAUDE.md §9). It exists because the fastest
    /// spelling of the "latest per market" query is SQLite-specific, and the
    /// thing worth testing is that it still agrees with the portable one.
    #[cfg(any(test, feature = "test-pool"))]
    pub fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }

    /// Open (creating if needed), configure and migrate the database.
    pub async fn connect(config: &SqliteConfig) -> RepoResult<Self> {
        let url = if config.is_memory() {
            // A shared cache keeps every pooled connection looking at the same
            // in-memory database; the name keeps separate ones separate.
            format!(
                "sqlite:file:memdb-{}?mode=memory&cache=shared",
                config.memory_name()
            )
        } else {
            if let Some(parent) = config.path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| RepoError::Backend(format!("creating {parent:?}: {e}")))?;
            }
            format!("sqlite://{}", config.path.display())
        };

        let options = SqliteConnectOptions::from_str(&url)
            .map_err(map_err)?
            .create_if_missing(true)
            .foreign_keys(true)
            // WAL keeps readers from blocking the writer.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_millis(config.busy_timeout_ms));

        let mut pool = SqlitePoolOptions::new().max_connections(config.max_connections);
        if let Some(ms) = config.max_lifetime_ms {
            pool = pool.max_lifetime(std::time::Duration::from_millis(ms));
        }
        if config.is_memory() {
            // A `mode=memory` database exists only while a connection to it is
            // open. The pool closes idle connections, so once the last one
            // went the database went with it -- and the next request opened a
            // fresh, empty one and failed with "no such table". Holding one
            // connection open for the pool's lifetime is what makes the shared
            // cache above actually shared.
            pool = pool
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None);
        }
        let pool = pool.connect_with(options).await.map_err(map_err)?;

        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .map_err(|e| RepoError::Backend(format!("migration failed: {e}")))?;

        analyze_if_never(&pool).await;

        tracing::info!(database = %url, "storage ready");

        Ok(Self {
            users: SqliteUsers::new(pool.clone()),
            sessions: SqliteSessions::new(pool.clone()),
            jobs: SqliteJobs::new(pool.clone()),
            events: SqliteEvents::new(pool.clone()),
            cache: SqliteCache::new(pool.clone()),
            kv: SqliteKv::new(pool.clone()),
            prices: SqlitePrices::new(pool.clone()),
            realm_prices: SqliteRealmPrices::new(pool.clone()),
            settings: SqliteSettings::new(pool.clone()),
            releases: SqliteReleases::new(pool.clone()),
            watches: SqliteWatches::new(pool.clone()),
            pool,
        })
    }

    /// The cluster runtime's view of storage, as one value.
    ///
    /// The runtime lives in its own task and needs its store by value; a
    /// repository is only a pool handle, so cloning costs nothing.
    pub fn cluster_handle(&self) -> SqliteClusterStore {
        SqliteClusterStore::new(self.pool.clone(), self.jobs.clone(), self.events.clone())
    }

    /// Refresh the query planner's statistics incrementally.
    ///
    /// `PRAGMA optimize` re-analyses only the tables whose shape has moved
    /// enough to matter, and `analysis_limit` caps how many rows it looks at
    /// per index, so this stays bounded however large the archive gets. Called
    /// from housekeeping: the price tables grow every collection cycle, and a
    /// plan chosen against last month's statistics is a plan chosen against
    /// the wrong table.
    pub async fn optimize(&self) {
        // The pragma is per connection, so both statements have to run on the
        // same one; a pool hands out whichever is free.
        let mut conn = match self.pool.acquire().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::debug!(error = %e, "no connection free to optimize on");
                return;
            }
        };
        for sql in ["PRAGMA analysis_limit = 400", "PRAGMA optimize"] {
            if let Err(e) = sqlx::query(sql).execute(&mut *conn).await {
                tracing::debug!(error = %e, sql, "optimize step failed");
                return;
            }
        }
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Give the query planner statistics, once, if it has never had any.
///
/// Not a micro-optimisation. Without `sqlite_stat1` SQLite guesses at how
/// selective each index is, and on the real archive it guessed wrong: the
/// category pages took **four times longer** -- 220ms against 53ms for
/// consumables, 129ms against 32ms for reagents -- purely because the plans
/// were chosen blind. The whole thing costs 200ms, once, at startup.
///
/// Only when there are no statistics at all. After that
/// [`SqliteStore::optimize`] keeps them current, incrementally, from
/// housekeeping.
///
/// Failure is logged and ignored: an app that will not start because it could
/// not gather statistics is worse than a slow one.
async fn analyze_if_never(pool: &Pool<Sqlite>) {
    let has_stats = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_stat1'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(1);

    if has_stats > 0 {
        return;
    }
    tracing::info!("gathering query planner statistics for the first time");
    if let Err(e) = sqlx::query("ANALYZE").execute(pool).await {
        tracing::warn!(error = %e, "ANALYZE failed; queries will use estimated plans");
    }
}

impl Store for SqliteStore {
    type Users = SqliteUsers;
    type Sessions = SqliteSessions;
    type Jobs = SqliteJobs;
    type Events = SqliteEvents;
    type Cache = SqliteCache;
    type Kv = SqliteKv;
    type Prices = SqlitePrices;
    type RealmPrices = SqliteRealmPrices;
    type Settings = SqliteSettings;
    type Watches = SqliteWatches;
    type Releases = SqliteReleases;

    fn users(&self) -> &Self::Users {
        &self.users
    }
    fn sessions(&self) -> &Self::Sessions {
        &self.sessions
    }
    fn jobs(&self) -> &Self::Jobs {
        &self.jobs
    }
    fn events(&self) -> &Self::Events {
        &self.events
    }
    fn cache(&self) -> &Self::Cache {
        &self.cache
    }
    fn kv(&self) -> &Self::Kv {
        &self.kv
    }
    fn prices(&self) -> &Self::Prices {
        &self.prices
    }
    fn watches(&self) -> &Self::Watches {
        &self.watches
    }
    fn realm_prices(&self) -> &Self::RealmPrices {
        &self.realm_prices
    }
    fn releases(&self) -> &Self::Releases {
        &self.releases
    }
    fn settings(&self) -> &Self::Settings {
        &self.settings
    }
}
