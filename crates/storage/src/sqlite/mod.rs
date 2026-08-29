//! SQLite implementation of the storage ports.

mod cache;
mod cluster;
mod events;
mod jobs;
mod kv;
mod prices;
mod realm_prices;
mod sessions;
mod users;

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
pub use sessions::SqliteSessions;
pub use users::SqliteUsers;

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
            max_connections: 4,
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
}

impl SqliteStore {
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

    pub async fn close(&self) {
        self.pool.close().await;
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
    fn realm_prices(&self) -> &Self::RealmPrices {
        &self.realm_prices
    }
}
