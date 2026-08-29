//! Per-realm gear prices.
//!
//! Deliberately a separate adapter from [`super::prices`]: the tables share no
//! column meanings, and one type implementing both ports would be the first
//! step towards a query that mixes a commodity's unit price with a gear
//! buyout.

use app_core::error::RepoResult;
use app_core::market::{Copper, ItemId, Realm, RealmId, RealmSample, Region};
use app_core::repo::RealmPriceRepository;
use cluster_core::Millis;
use sqlx::sqlite::SqliteRow;
use sqlx::{Pool, Row, Sqlite};

use super::{corrupt, map_err};

#[derive(Clone)]
pub struct SqliteRealmPrices {
    pool: Pool<Sqlite>,
}

impl SqliteRealmPrices {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

fn sample_from_row(row: &SqliteRow) -> RepoResult<RealmSample> {
    let region: String = row.get("region");
    Ok(RealmSample {
        item: ItemId(row.get::<i64, _>("item_id") as u32),
        region: Region::parse(&region).ok_or_else(|| corrupt("region", region))?,
        realm: RealmId(row.get::<i64, _>("realm_id") as u32),
        variant: row.get("variant"),
        observed_at: Millis(row.get::<i64, _>("observed_at") as u64),
        min_price: Copper(row.get::<i64, _>("min_price") as u64),
        median_price: Copper(row.get::<i64, _>("median_price") as u64),
        max_price: Copper(row.get::<i64, _>("max_price") as u64),
        listings: row.get::<i64, _>("listings") as u32,
    })
}

/// The newest row per (item, realm, variant), for a whole region or one realm.
///
/// A window function rather than a correlated subquery: one pass over the
/// index instead of one lookup per group, which matters because the
/// cross-realm view runs this on every page load.
const LATEST: &str = "
    SELECT item_id, region, realm_id, variant, observed_at,
           min_price, median_price, max_price, listings
      FROM (
        SELECT *, ROW_NUMBER() OVER (
                    PARTITION BY item_id, realm_id, variant
                    ORDER BY observed_at DESC) AS rn
          FROM realm_price_samples
         WHERE region = ?";

impl RealmPriceRepository for SqliteRealmPrices {
    /// One transaction per snapshot: a realm's hour lands whole or not at all.
    async fn record_samples(&self, samples: &[RealmSample]) -> RepoResult<u64> {
        if samples.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0u64;
        for sample in samples {
            let result = sqlx::query(
                "INSERT INTO realm_price_samples
                   (item_id, region, realm_id, variant, observed_at,
                    min_price, median_price, max_price, listings)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(item_id, region, realm_id, variant, observed_at) DO NOTHING",
            )
            .bind(sample.item.get() as i64)
            .bind(sample.region.as_str())
            .bind(sample.realm.get() as i64)
            .bind(&sample.variant)
            .bind(sample.observed_at.get() as i64)
            .bind(sample.min_price.get() as i64)
            .bind(sample.median_price.get() as i64)
            .bind(sample.max_price.get() as i64)
            .bind(sample.listings as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            written += result.rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn latest(&self, region: Region, realm: RealmId) -> RepoResult<Vec<RealmSample>> {
        let sql = format!("{LATEST} AND realm_id = ? ) WHERE rn = 1");
        let rows = sqlx::query(&sql)
            .bind(region.as_str())
            .bind(realm.get() as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn latest_in_region(&self, region: Region) -> RepoResult<Vec<RealmSample>> {
        let sql = format!("{LATEST} ) WHERE rn = 1");
        let rows = sqlx::query(&sql)
            .bind(region.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn history(
        &self,
        item: ItemId,
        region: Region,
        realm: RealmId,
        since: Millis,
    ) -> RepoResult<Vec<RealmSample>> {
        let rows = sqlx::query(
            "SELECT item_id, region, realm_id, variant, observed_at,
                    min_price, median_price, max_price, listings
               FROM realm_price_samples
              WHERE item_id = ? AND region = ? AND realm_id = ? AND observed_at >= ?
              ORDER BY observed_at",
        )
        .bind(item.get() as i64)
        .bind(region.as_str())
        .bind(realm.get() as i64)
        .bind(since.get() as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn history_in_region(
        &self,
        item: ItemId,
        region: Region,
        since: Millis,
    ) -> RepoResult<Vec<RealmSample>> {
        let rows = sqlx::query(
            "SELECT item_id, region, realm_id, variant, observed_at,
                    min_price, median_price, max_price, listings
               FROM realm_price_samples
              WHERE item_id = ? AND region = ? AND observed_at >= ?
              ORDER BY observed_at",
        )
        .bind(item.get() as i64)
        .bind(region.as_str())
        .bind(since.get() as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn last_observed(&self, region: Region, realm: RealmId) -> RepoResult<Option<Millis>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT MAX(observed_at) FROM realm_price_samples WHERE region = ? AND realm_id = ?",
        )
        .bind(region.as_str())
        .bind(realm.get() as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(row.map(|(at,)| Millis(at as u64)).filter(|m| m.get() > 0))
    }

    /// As [`super::prices`], but a gear market is keyed by realm and variant
    /// too. The cheapest and the dearest of the day both survive: on one realm
    /// the spread is the only comparison there is.
    async fn downsample_before(&self, before: Millis) -> RepoResult<u64> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        sqlx::query(
            "INSERT INTO realm_price_samples
                 (item_id, region, realm_id, variant, observed_at,
                  min_price, median_price, max_price, listings)
             SELECT item_id, region, realm_id, variant,
                    (observed_at / 86400000) * 86400000,
                    MIN(min_price),
                    CAST(AVG(median_price) AS INTEGER),
                    MAX(max_price),
                    CAST(AVG(listings) AS INTEGER)
               FROM realm_price_samples
              WHERE observed_at < ?
              GROUP BY item_id, region, realm_id, variant, (observed_at / 86400000)
             ON CONFLICT(item_id, region, realm_id, variant, observed_at) DO UPDATE SET
                    min_price    = excluded.min_price,
                    median_price = excluded.median_price,
                    max_price    = excluded.max_price,
                    listings     = excluded.listings",
        )
        .bind(before.get() as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        let removed = sqlx::query(
            "DELETE FROM realm_price_samples
              WHERE observed_at < ? AND observed_at % 86400000 != 0",
        )
        .bind(before.get() as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        tx.commit().await.map_err(map_err)?;
        Ok(removed.rows_affected())
    }

    /// Remember a realm, without overriding whether it is collected.
    ///
    /// Discovery runs at every startup and must not undo an operator's choice:
    /// a realm switched off in the admin page stays off when the upstream
    /// index mentions it again tomorrow.
    async fn record_realm(&self, realm: &Realm) -> RepoResult<()> {
        sqlx::query(
            "INSERT INTO realms (realm_id, region, name, enabled) VALUES (?, ?, ?, ?)
             ON CONFLICT(realm_id, region) DO UPDATE SET name = excluded.name",
        )
        .bind(realm.id.get() as i64)
        .bind(realm.region.as_str())
        .bind(&realm.name)
        .bind(realm.enabled as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn set_realm_enabled(
        &self,
        region: Region,
        realm: RealmId,
        enabled: bool,
    ) -> RepoResult<()> {
        sqlx::query("UPDATE realms SET enabled = ? WHERE realm_id = ? AND region = ?")
            .bind(enabled as i64)
            .bind(realm.get() as i64)
            .bind(region.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn realms(&self) -> RepoResult<Vec<Realm>> {
        let rows =
            sqlx::query("SELECT realm_id, region, name, enabled FROM realms ORDER BY region, name")
                .fetch_all(&self.pool)
                .await
                .map_err(map_err)?;
        rows.iter()
            .map(|row| {
                let region: String = row.get("region");
                Ok(Realm {
                    id: RealmId(row.get::<i64, _>("realm_id") as u32),
                    region: Region::parse(&region).ok_or_else(|| corrupt("region", region))?,
                    name: row.get("name"),
                    enabled: row.get::<i64, _>("enabled") != 0,
                })
            })
            .collect()
    }
}
