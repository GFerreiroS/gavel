use app_core::error::RepoResult;
use app_core::repo::KeyValueStore;
use sqlx::{Pool, Row, Sqlite};

use super::{map_err, write_guard};

#[derive(Clone)]
pub struct SqliteKv {
    pool: Pool<Sqlite>,
}

impl SqliteKv {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

impl KeyValueStore for SqliteKv {
    async fn get(&self, key: &str) -> RepoResult<Option<Vec<u8>>> {
        let row = sqlx::query("SELECT value FROM kv WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>("value")))
    }

    async fn put(&self, key: &str, value: &[u8]) -> RepoResult<()> {
        let _write = write_guard("key-value update").await;
        sqlx::query(
            "INSERT INTO kv(key, value, updated_at) VALUES(?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> RepoResult<()> {
        let _write = write_guard("key-value delete").await;
        sqlx::query("DELETE FROM kv WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

/// The KV port has no clock parameter, so the bookkeeping column is filled in
/// locally. It is metadata only -- nothing reads it for correctness.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}
