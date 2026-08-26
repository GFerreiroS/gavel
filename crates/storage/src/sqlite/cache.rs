use app_core::error::RepoResult;
use app_core::repo::CacheStore;
use cluster_core::Millis;
use sqlx::{Pool, Row, Sqlite};

use super::map_err;

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

    async fn put(&self, key: &str, value: &[u8], expires_at: Millis) -> RepoResult<()> {
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
        let result = sqlx::query("DELETE FROM cache WHERE expires_at <= ?")
            .bind(now.get() as i64)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(result.rows_affected())
    }
}
