//! What the tracker collects.
//!
//! Two rows' worth of state, but its own adapter: it is the only table the web
//! layer *writes* outside of authentication, and mixing it into the price
//! repositories would put a settings update inside a type whose other methods
//! are all hot-path reads.

use app_core::error::RepoResult;
use app_core::repo::SettingsRepository;
use sqlx::{Pool, Row, Sqlite};

use super::{map_err, write_guard};

#[derive(Clone)]
pub struct SqliteSettings {
    pool: Pool<Sqlite>,
}

impl SqliteSettings {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

impl SettingsRepository for SqliteSettings {
    async fn set_enabled(&self, name: &str, enabled: bool) -> RepoResult<()> {
        let _write = write_guard("collection setting").await;
        sqlx::query(
            "INSERT INTO collection_settings (name, enabled) VALUES (?, ?)
             ON CONFLICT(name) DO UPDATE SET enabled = excluded.enabled",
        )
        .bind(name)
        .bind(enabled as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn disabled(&self) -> RepoResult<Vec<String>> {
        let rows = sqlx::query("SELECT name FROM collection_settings WHERE enabled = 0")
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(rows
            .iter()
            .map(|row| row.get::<String, _>("name"))
            .collect())
    }
}
