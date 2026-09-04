use app_core::error::RepoResult;
use app_core::repo::CacheStore;
use cluster_core::Millis;
use sqlx::{Pool, Row, Sqlite};

use super::{map_err, write_guard};

/// Keys per statement. Well under SQLite's parameter limit on every build,
/// and large enough that a full catalogue is one or two round trips.
const CHUNK: usize = 400;

#[derive(Clone)]
pub struct SqliteCache {
    pool: Pool<Sqlite>,
}

impl SqliteCache {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

impl CacheStore for SqliteCache {
    async fn get(&self, key: &str, now: Millis) -> RepoResult<Option<Vec<u8>>> {
        let row = sqlx::query("SELECT value FROM cache WHERE key = ? AND expires_at > ?")
            .bind(key)
            .bind(now.get() as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>("value")))
    }

    async fn get_many(&self, keys: &[String], now: Millis) -> RepoResult<Vec<(String, Vec<u8>)>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut found = Vec::with_capacity(keys.len());
        // SQLite caps how many parameters one statement may bind, and a
        // catalogue is comfortably larger than the smallest builds allow. The
        // chunk keeps this one query rather than one per key without betting
        // on a limit that varies by build.
        for chunk in keys.chunks(CHUNK) {
            // The placeholders are generated, the values are still bound: the
            // only thing interpolated here is a string of `?`s whose length
            // comes from `chunk.len()`.
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT key, value FROM cache WHERE key IN ({placeholders}) AND expires_at > ?"
            );
            let mut query = sqlx::query(&sql);
            for key in chunk {
                query = query.bind(key);
            }
            let rows = query
                .bind(now.get() as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(map_err)?;
            found.extend(
                rows.into_iter()
                    .map(|r| (r.get::<String, _>("key"), r.get::<Vec<u8>, _>("value"))),
            );
        }
        Ok(found)
    }

    async fn put(&self, key: &str, value: &[u8], expires_at: Millis) -> RepoResult<()> {
        let _write = write_guard("cache update").await;
        sqlx::query(
            "INSERT INTO cache(key, value, expires_at) VALUES(?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
        )
        .bind(key)
        .bind(value)
        .bind(expires_at.get() as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn purge_expired(&self, now: Millis) -> RepoResult<u64> {
        let _write = write_guard("cache pruning").await;
        let result = sqlx::query("DELETE FROM cache WHERE expires_at <= ?")
            .bind(now.get() as i64)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(result.rows_affected())
    }
}
