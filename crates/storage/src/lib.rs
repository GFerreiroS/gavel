//! Persistence adapters.
//!
//! Nothing above this crate sees `sqlx`: every public item speaks in
//! `app_core` and `cluster_core` types and returns `RepoResult`.
#![forbid(unsafe_code)]

mod sqlite;

pub use sqlite::{
    SqliteCache, SqliteClusterStore, SqliteConfig, SqliteEvents, SqliteJobs, SqliteKv,
    SqlitePrices, SqliteSessions, SqliteStore, SqliteUsers,
};
