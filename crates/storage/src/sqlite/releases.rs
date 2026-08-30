//! Where each catalogue is in its life.
//!
//! The interesting method is [`SqliteReleases::activate`], and what is
//! interesting about it is that it is one transaction. `docs/market-analysis`
//! §8 asks for activation and archiving to happen together, and the reason is
//! that both ways of getting it wrong are worse than the change not happening:
//! two active catalogues means two things collecting into one archive, and
//! none means the front page has nothing to show.

use app_core::error::{RepoError, RepoResult};
use app_core::market::catalog::CatalogStatus;
use app_core::repo::{Activation, Release, ReleaseRepository};
use cluster_core::Millis;
use sqlx::{Pool, Row, Sqlite};

use super::{corrupt, map_err};

pub struct SqliteReleases {
    pool: Pool<Sqlite>,
}

impl SqliteReleases {
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

fn millis(value: Option<i64>) -> Option<Millis> {
    value.map(|v| Millis(v as u64))
}

impl ReleaseRepository for SqliteReleases {
    async fn releases(&self) -> RepoResult<Vec<Release>> {
        let rows = sqlx::query(
            "SELECT catalog_id, state, changed_at, activated_at, archived_at
               FROM catalog_releases
              ORDER BY catalog_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;

        rows.into_iter()
            .map(|row| {
                let state: String = row.get("state");
                Ok(Release {
                    catalog: row.get("catalog_id"),
                    // A state the schema's CHECK allows but this binary does
                    // not know is a downgrade, and guessing which of the three
                    // it meant would be worse than saying the row is corrupt.
                    state: CatalogStatus::parse(&state)
                        .ok_or_else(|| corrupt("catalog release state", state))?,
                    changed_at: Millis(row.get::<i64, _>("changed_at") as u64),
                    activated_at: millis(row.get("activated_at")),
                    archived_at: millis(row.get("archived_at")),
                })
            })
            .collect()
    }

    async fn seed(&self, defaults: &[(String, CatalogStatus)], now: Millis) -> RepoResult<u64> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0u64;
        for (catalog, state) in defaults {
            // `DO NOTHING`, not `DO UPDATE`. A state a person set outranks the
            // one the binary shipped with, or an upgrade would silently undo
            // an activation -- and the whole point of moving this out of the
            // JSON was that a person, not a build, decides it.
            let result = sqlx::query(
                "INSERT INTO catalog_releases (catalog_id, state, changed_at, activated_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(catalog_id) DO NOTHING",
            )
            .bind(catalog)
            .bind(state.as_str())
            .bind(now.get() as i64)
            .bind(state.is_active().then_some(now.get() as i64))
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            written += result.rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn activate(&self, catalog: &str, now: Millis) -> RepoResult<Activation> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;

        // The row has to exist: activating a catalogue this instance has never
        // heard of would create one with no content behind it.
        let present: Option<(String,)> =
            sqlx::query_as("SELECT state FROM catalog_releases WHERE catalog_id = ?")
                .bind(catalog)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_err)?;
        let Some((state,)) = present else {
            return Err(RepoError::NotFound);
        };

        // Pressing the button twice is not a fault, and must not archive the
        // catalogue it just activated.
        if state == CatalogStatus::Active.as_str() {
            tx.commit().await.map_err(map_err)?;
            return Ok(Activation {
                activated: catalog.to_string(),
                archived: None,
            });
        }

        let previous: Option<(String,)> = sqlx::query_as(
            "SELECT catalog_id FROM catalog_releases WHERE state = 'active' LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_err)?;

        // Archive first. The unique index allows one active row, so setting
        // the new one first would collide with the old one and fail the whole
        // transaction -- which is safe, but this way round it simply works.
        if let Some((ref old,)) = previous {
            sqlx::query(
                "UPDATE catalog_releases
                    SET state = 'archived', changed_at = ?, archived_at = ?
                  WHERE catalog_id = ?",
            )
            .bind(now.get() as i64)
            .bind(now.get() as i64)
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        }

        sqlx::query(
            "UPDATE catalog_releases
                SET state = 'active', changed_at = ?, activated_at = ?, archived_at = NULL
              WHERE catalog_id = ?",
        )
        .bind(now.get() as i64)
        .bind(now.get() as i64)
        .bind(catalog)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        tx.commit().await.map_err(map_err)?;
        Ok(Activation {
            activated: catalog.to_string(),
            archived: previous.map(|(id,)| id),
        })
    }
}
