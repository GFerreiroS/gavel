use app_core::error::RepoResult;
use app_core::market::{ItemId, Region};
use app_core::model::UserId;
use app_core::repo::{Watch, WatchRepository};
use cluster_core::Millis;
use sqlx::{Pool, Row, Sqlite};

use super::map_err;

#[derive(Clone)]
pub struct SqliteWatches {
    pool: Pool<Sqlite>,
}

impl SqliteWatches {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

impl WatchRepository for SqliteWatches {
    async fn watches(&self, user: UserId) -> RepoResult<Vec<Watch>> {
        let rows = sqlx::query(
            "SELECT item_id, region, added_at FROM user_watches
             WHERE user_id = ? ORDER BY added_at DESC",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;

        Ok(rows
            .iter()
            .filter_map(|row| {
                // A region this build no longer knows is dropped rather than
                // failing the whole read: one stale row must not take the
                // page down with it.
                Some(Watch {
                    item: ItemId(row.get::<i64, _>("item_id") as u32),
                    region: Region::parse(&row.get::<String, _>("region"))?,
                    added_at: Millis(row.get::<i64, _>("added_at") as u64),
                })
            })
            .collect())
    }

    async fn watch(
        &self,
        user: UserId,
        item: ItemId,
        region: Region,
        now: Millis,
    ) -> RepoResult<()> {
        // Idempotent: the control is a toggle and a double-click is not a
        // fault. Keeping the original `added_at` means the list does not
        // reshuffle when somebody clicks twice.
        sqlx::query(
            "INSERT INTO user_watches(user_id, item_id, region, added_at)
             VALUES(?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(user)
        .bind(item.get() as i64)
        .bind(region.as_str())
        .bind(now.get() as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn unwatch(&self, user: UserId, item: ItemId, region: Region) -> RepoResult<()> {
        sqlx::query("DELETE FROM user_watches WHERE user_id = ? AND item_id = ? AND region = ?")
            .bind(user)
            .bind(item.get() as i64)
            .bind(region.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}
