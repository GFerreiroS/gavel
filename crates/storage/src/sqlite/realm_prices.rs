//! Per-realm gear prices.
//!
//! Deliberately a separate adapter from [`super::prices`]: the tables share no
//! column meanings, and one type implementing both ports would be the first
//! step towards a query that mixes a commodity's unit price with a gear
//! buyout.

use std::collections::BTreeMap;

use app_core::error::RepoResult;
use app_core::market::{Copper, ItemId, Ladder, Realm, RealmId, RealmSample, Region};
use app_core::repo::RealmPriceRepository;
use cluster_core::Millis;
use sqlx::sqlite::SqliteRow;
use sqlx::{Pool, Row, Sqlite, Transaction};

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

/// Resolve the storage-only dictionary key while keeping the domain boundary
/// expressed as the stable, full bonus-list string.
async fn variant_id(
    tx: &mut Transaction<'_, Sqlite>,
    cache: &mut BTreeMap<String, i64>,
    variant: &str,
) -> RepoResult<i64> {
    if let Some(id) = cache.get(variant) {
        return Ok(*id);
    }

    sqlx::query(
        "INSERT INTO market_variants (variant) VALUES (?)
         ON CONFLICT(variant) DO NOTHING",
    )
    .bind(variant)
    .execute(&mut **tx)
    .await
    .map_err(map_err)?;

    let id = sqlx::query_scalar("SELECT variant_id FROM market_variants WHERE variant = ?")
        .bind(variant)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_err)?;
    cache.insert(variant.to_owned(), id);
    Ok(id)
}

/// The newest row per (item, realm, variant), for a whole region or one realm.
///
/// The newest row for every market, which is what a price page is.
///
/// `MAX(observed_at)` with the other columns bare. SQLite guarantees that when
/// a query has exactly one aggregate and it is `min()` or `max()`, the bare
/// columns come from the row that produced it -- so this is one row per group,
/// and it is *that* group's newest row rather than an arbitrary one.
///
/// This replaced a `ROW_NUMBER() OVER (PARTITION BY ...)`, which is the
/// portable spelling and was four and a half times slower on the real
/// archive: 104ms against 23ms for one region's 18,407 markets, because the
/// window has to number every row it passes and this stops at the first of
/// each group. `latest_matches_the_window_function` holds the two against each
/// other on real data.
///
/// It is SQLite-specific, and this file is the SQLite adapter. A Postgres
/// adapter would reject the query outright rather than answer it differently,
/// which is the failure mode to want.
const LATEST: &str = "
    SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
           MAX(samples.observed_at) AS observed_at,
           samples.min_price, samples.median_price, samples.max_price, samples.listings
      FROM realm_price_samples AS samples
      JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
     WHERE samples.region = ?";

/// Closes [`LATEST`], grouping by what makes a market a market.
const BY_MARKET: &str = " GROUP BY samples.item_id, samples.realm_id, samples.variant_id";

impl RealmPriceRepository for SqliteRealmPrices {
    async fn record_snapshot(
        &self,
        samples: &[RealmSample],
        region: Region,
        realm: RealmId,
        observed_at: Millis,
        ladders: &[(ItemId, String, Ladder)],
    ) -> RepoResult<(u64, u64)> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        // This is the durable evidence that the realm was fetched.  It stays
        // append-only even while Slice A still writes every state row: later
        // change detection needs to distinguish an observed unchanged snapshot
        // from a collection gap without inferring anything before migration 30.
        sqlx::query(
            "INSERT OR IGNORE INTO collection_snapshots (region, realm_id, observed_at)
             VALUES (?, ?, ?)",
        )
        .bind(region.as_str())
        .bind(realm.get() as i64)
        .bind(observed_at.get() as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        let mut variants = std::collections::BTreeMap::new();

        // Fetch previous state to suppress unchanged rows.
        let previous_rows: Vec<(i64, i64, i64, i64, i64, Option<String>)> = sqlx::query_as(
            "SELECT samples.item_id, samples.variant_id, samples.min_price,
                    samples.median_price, samples.listings, ladders.steps
               FROM realm_price_samples AS samples
               LEFT JOIN realm_price_ladders AS ladders
                 ON ladders.item_id = samples.item_id
                AND ladders.region = samples.region
                AND ladders.realm_id = samples.realm_id
                AND ladders.variant_id = samples.variant_id
                AND ladders.observed_at = samples.observed_at
              WHERE samples.region = ? AND samples.realm_id = ?
                AND samples.observed_at = (
                    SELECT MAX(previous.observed_at) FROM realm_price_samples AS previous
                     WHERE previous.region = samples.region AND previous.realm_id = samples.realm_id
                       AND previous.item_id = samples.item_id AND previous.variant_id = samples.variant_id
                )"
        )
        .bind(region.as_str())
        .bind(realm.get() as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_err)?;

        #[allow(clippy::type_complexity)]
        let mut previous: std::collections::HashMap<
            (i64, i64),
            (i64, i64, i64, Option<String>),
        > = previous_rows
            .into_iter()
            .map(|(item, variant, min_p, med_p, list, steps)| {
                ((item, variant), (min_p, med_p, list, steps))
            })
            .collect();

        // Encode incoming ladders
        let mut incoming_ladders: std::collections::HashMap<(i64, &str), String> =
            std::collections::HashMap::new();
        for (item, variant, ladder) in ladders {
            if !ladder.is_empty() {
                incoming_ladders.insert((item.get() as i64, variant.as_str()), ladder.encode());
            }
        }

        let mut sample_rows = 0u64;
        let mut ladder_rows = 0u64;

        for sample in samples {
            let item_id = sample.item.get() as i64;
            let variant_id = variant_id(&mut tx, &mut variants, &sample.variant).await?;

            let min_price = sample.min_price.get() as i64;
            let median_price = sample.median_price.get() as i64;
            let max_price = sample.max_price.get() as i64;
            let listings = sample.listings as i64;
            let incoming_steps = incoming_ladders
                .get(&(item_id, sample.variant.as_str()))
                .cloned();

            let changed = match previous.remove(&(item_id, variant_id)) {
                Some((prev_min, prev_med, prev_list, prev_steps)) => {
                    min_price != prev_min
                        || median_price != prev_med
                        || listings != prev_list
                        || incoming_steps != prev_steps
                }
                None => true,
            };

            if changed {
                sample_rows += sqlx::query(
                    "INSERT INTO realm_price_samples
                       (item_id, region, realm_id, variant_id, observed_at,
                        min_price, median_price, max_price, listings)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(item_id, region, realm_id, variant_id, observed_at) DO NOTHING",
                )
                .bind(item_id)
                .bind(region.as_str())
                .bind(realm.get() as i64)
                .bind(variant_id)
                .bind(observed_at.get() as i64)
                .bind(min_price)
                .bind(median_price)
                .bind(max_price)
                .bind(listings)
                .execute(&mut *tx)
                .await
                .map_err(map_err)?
                .rows_affected();

                // Write ladders only if the market changed (which means either ladder or sample changed)
                if let Some((item, _variant, ladder)) = ladders
                    .iter()
                    .find(|(i, v, _l)| {
                        i.get() as i64 == item_id && v.as_str() == sample.variant.as_str()
                    })
                    .filter(|(_, _, l)| !l.is_empty())
                {
                    ladder_rows += sqlx::query(
                            "INSERT OR IGNORE INTO realm_price_ladders
                               (item_id, region, realm_id, variant_id, observed_at, levels, total, steps)
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                        )
                        .bind(item.get() as i64)
                        .bind(region.as_str())
                        .bind(realm.get() as i64)
                        .bind(variant_id)
                        .bind(observed_at.get() as i64)
                        .bind(ladder.levels() as i64)
                        .bind(ladder.total() as i64)
                        .bind(incoming_steps.unwrap())
                        .execute(&mut *tx)
                        .await
                        .map_err(map_err)?
                        .rows_affected();
                }
            }
        }

        // Tombstones for disappeared markets
        for ((item_id, variant_id), (_, _, list, _)) in previous {
            if list > 0 {
                sample_rows += sqlx::query(
                    "INSERT INTO realm_price_samples
                       (item_id, region, realm_id, variant_id, observed_at,
                        min_price, median_price, max_price, listings)
                     VALUES (?, ?, ?, ?, ?, 0, 0, 0, 0)
                     ON CONFLICT(item_id, region, realm_id, variant_id, observed_at) DO NOTHING",
                )
                .bind(item_id)
                .bind(region.as_str())
                .bind(realm.get() as i64)
                .bind(variant_id)
                .bind(observed_at.get() as i64)
                .execute(&mut *tx)
                .await
                .map_err(map_err)?
                .rows_affected();
            }
        }

        tx.commit().await.map_err(map_err)?;
        Ok((sample_rows, ladder_rows))
    }

    /// One transaction per snapshot: a realm's hour lands whole or not at all.
    async fn record_samples(&self, samples: &[RealmSample]) -> RepoResult<u64> {
        if samples.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut variants = BTreeMap::new();
        let mut written = 0u64;
        for sample in samples {
            let variant_id = variant_id(&mut tx, &mut variants, &sample.variant).await?;
            let result = sqlx::query(
                "INSERT INTO realm_price_samples
                   (item_id, region, realm_id, variant_id, observed_at,
                    min_price, median_price, max_price, listings)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(item_id, region, realm_id, variant_id, observed_at) DO NOTHING",
            )
            .bind(sample.item.get() as i64)
            .bind(sample.region.as_str())
            .bind(sample.realm.get() as i64)
            .bind(variant_id)
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

    async fn record_ladders(
        &self,
        region: Region,
        realm: RealmId,
        observed_at: Millis,
        ladders: &[(ItemId, String, Ladder)],
    ) -> RepoResult<u64> {
        if ladders.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut variants = BTreeMap::new();
        let mut written = 0u64;
        for (item, variant, ladder) in ladders {
            if ladder.is_empty() {
                continue;
            }
            let variant_id = variant_id(&mut tx, &mut variants, variant).await?;
            let result = sqlx::query(
                "INSERT OR IGNORE INTO realm_price_ladders
                   (item_id, region, realm_id, variant_id, observed_at, levels, total, steps)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(item.get() as i64)
            .bind(region.as_str())
            .bind(realm.get() as i64)
            .bind(variant_id)
            .bind(observed_at.get() as i64)
            .bind(ladder.levels() as i64)
            .bind(ladder.total() as i64)
            .bind(ladder.encode())
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            written += result.rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn latest_ladders_for(
        &self,
        region: Region,
        item: ItemId,
    ) -> RepoResult<Vec<(RealmId, String, Millis, Ladder)>> {
        let rows = sqlx::query(
            "SELECT ladders.realm_id, variants.variant,
                    max(ladders.observed_at) AS observed_at, ladders.steps
               FROM realm_price_ladders AS ladders
               JOIN market_variants AS variants ON variants.variant_id = ladders.variant_id
              WHERE ladders.region = ? AND ladders.item_id = ?
              GROUP BY ladders.realm_id, ladders.variant_id",
        )
        .bind(region.as_str())
        .bind(item.get() as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    RealmId(row.get::<i64, _>("realm_id") as u32),
                    row.get::<String, _>("variant"),
                    Millis(row.get::<i64, _>("observed_at") as u64),
                    Ladder::decode(&row.get::<String, _>("steps")),
                )
            })
            .collect())
    }

    async fn latest_ladders_in_region(
        &self,
        region: Region,
    ) -> RepoResult<Vec<(RealmId, ItemId, String, Ladder)>> {
        // The same shape as `latest_ladders_for` without the item filter, and
        // the same `max(observed_at)` with bare columns §11b records as a
        // SQLite promise rather than SQL.
        let rows = sqlx::query(
            "SELECT ladders.realm_id, ladders.item_id, variants.variant,
                    max(ladders.observed_at) AS observed_at, ladders.steps
               FROM realm_price_ladders AS ladders
               JOIN market_variants AS variants ON variants.variant_id = ladders.variant_id
              WHERE ladders.region = ?
              GROUP BY ladders.realm_id, ladders.item_id, ladders.variant_id",
        )
        .bind(region.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    RealmId(row.get::<i64, _>("realm_id") as u32),
                    ItemId(row.get::<i64, _>("item_id") as u32),
                    row.get::<String, _>("variant"),
                    Ladder::decode(&row.get::<String, _>("steps")),
                )
            })
            .collect())
    }

    async fn prune_ladders_before(&self, before: Millis) -> RepoResult<u64> {
        let result = sqlx::query("DELETE FROM realm_price_ladders WHERE observed_at < ?")
            .bind(before.get() as i64)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(result.rows_affected())
    }

    async fn latest(&self, region: Region, realm: RealmId) -> RepoResult<Vec<RealmSample>> {
        let sql = format!("{LATEST} AND samples.realm_id = ?{BY_MARKET}");
        let rows = sqlx::query(&sql)
            .bind(region.as_str())
            .bind(realm.get() as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn latest_in_region(&self, region: Region) -> RepoResult<Vec<RealmSample>> {
        let sql = format!("{LATEST}{BY_MARKET}");
        let rows = sqlx::query(&sql)
            .bind(region.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn window_in_region(
        &self,
        region: Region,
        since: Millis,
    ) -> RepoResult<Vec<RealmSample>> {
        // Post-seam ledger instants expand from the state current at that
        // instant. Pre-seam rows have no ledger evidence and remain raw: they
        // are history, not a claim that an otherwise unrecorded hour was seen.
        let rows = sqlx::query(
            "WITH expanded AS (
                 SELECT samples.item_id, samples.region, samples.realm_id, samples.variant_id,
                        snapshots.observed_at, samples.min_price, samples.median_price,
                        samples.max_price, samples.listings
                   FROM collection_snapshots AS snapshots
                   JOIN realm_price_samples AS samples
                     ON samples.region = snapshots.region AND samples.realm_id = snapshots.realm_id
                  WHERE snapshots.region = ? AND snapshots.observed_at >= ?
                    AND samples.observed_at = (
                        SELECT MAX(previous.observed_at) FROM realm_price_samples AS previous
                         WHERE previous.region = snapshots.region
                           AND previous.realm_id = snapshots.realm_id
                           AND previous.item_id = samples.item_id
                           AND previous.variant_id = samples.variant_id
                           AND previous.observed_at <= snapshots.observed_at
                    )
             )
             SELECT expanded.item_id, expanded.region, expanded.realm_id, variants.variant,
                    expanded.observed_at, expanded.min_price, expanded.median_price,
                    expanded.max_price, expanded.listings
               FROM expanded JOIN market_variants AS variants ON variants.variant_id = expanded.variant_id
              WHERE expanded.listings > 0
             UNION ALL
             SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
                    samples.observed_at, samples.min_price, samples.median_price,
                    samples.max_price, samples.listings
               FROM realm_price_samples AS samples
               JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
              WHERE samples.region = ? AND samples.observed_at >= ?
                AND NOT EXISTS (
                    SELECT 1 FROM collection_snapshots AS snapshots
                     WHERE snapshots.region = samples.region AND snapshots.realm_id = samples.realm_id
                       AND snapshots.observed_at = samples.observed_at
                )
              ORDER BY item_id, realm_id, variant, observed_at",
        )
        .bind(region.as_str())
        .bind(since.get() as i64)
        .bind(region.as_str())
        .bind(since.get() as i64)
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
            "WITH expanded AS (
                 SELECT samples.item_id, samples.region, samples.realm_id, samples.variant_id,
                        snapshots.observed_at, samples.min_price, samples.median_price, samples.max_price, samples.listings
                   FROM collection_snapshots AS snapshots JOIN realm_price_samples AS samples
                     ON samples.region = snapshots.region AND samples.realm_id = snapshots.realm_id
                  WHERE snapshots.region = ? AND snapshots.observed_at >= ?
                    AND samples.item_id = ? AND samples.realm_id = ?
                    AND samples.observed_at = (SELECT MAX(previous.observed_at) FROM realm_price_samples AS previous
                         WHERE previous.region = snapshots.region AND previous.realm_id = snapshots.realm_id
                           AND previous.item_id = samples.item_id AND previous.variant_id = samples.variant_id
                           AND previous.observed_at <= snapshots.observed_at)
             )
             SELECT expanded.item_id, expanded.region, expanded.realm_id, variants.variant, expanded.observed_at,
                    expanded.min_price, expanded.median_price, expanded.max_price, expanded.listings
               FROM expanded JOIN market_variants AS variants ON variants.variant_id = expanded.variant_id
              WHERE expanded.listings > 0
             UNION ALL
             SELECT samples.item_id, samples.region, samples.realm_id, variants.variant, samples.observed_at,
                    samples.min_price, samples.median_price, samples.max_price, samples.listings
               FROM realm_price_samples AS samples JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
              WHERE samples.region = ? AND samples.observed_at >= ? AND samples.item_id = ? AND samples.realm_id = ?
                AND NOT EXISTS (SELECT 1 FROM collection_snapshots AS snapshots WHERE snapshots.region = samples.region
                                  AND snapshots.realm_id = samples.realm_id AND snapshots.observed_at = samples.observed_at)
              ORDER BY observed_at",
        )
        .bind(region.as_str()).bind(since.get() as i64).bind(item.get() as i64).bind(realm.get() as i64)
        .bind(region.as_str()).bind(since.get() as i64).bind(item.get() as i64).bind(realm.get() as i64)
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
            "WITH expanded AS (
                 SELECT samples.item_id, samples.region, samples.realm_id, samples.variant_id,
                        snapshots.observed_at, samples.min_price, samples.median_price, samples.max_price, samples.listings
                   FROM collection_snapshots AS snapshots JOIN realm_price_samples AS samples
                     ON samples.region = snapshots.region AND samples.realm_id = snapshots.realm_id
                  WHERE snapshots.region = ? AND snapshots.observed_at >= ? AND samples.item_id = ?
                    AND samples.observed_at = (SELECT MAX(previous.observed_at) FROM realm_price_samples AS previous
                         WHERE previous.region = snapshots.region AND previous.realm_id = snapshots.realm_id
                           AND previous.item_id = samples.item_id AND previous.variant_id = samples.variant_id
                           AND previous.observed_at <= snapshots.observed_at)
             )
             SELECT expanded.item_id, expanded.region, expanded.realm_id, variants.variant, expanded.observed_at,
                    expanded.min_price, expanded.median_price, expanded.max_price, expanded.listings
               FROM expanded JOIN market_variants AS variants ON variants.variant_id = expanded.variant_id
              WHERE expanded.listings > 0
             UNION ALL
             SELECT samples.item_id, samples.region, samples.realm_id, variants.variant, samples.observed_at,
                    samples.min_price, samples.median_price, samples.max_price, samples.listings
               FROM realm_price_samples AS samples JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
              WHERE samples.region = ? AND samples.observed_at >= ? AND samples.item_id = ?
                AND NOT EXISTS (SELECT 1 FROM collection_snapshots AS snapshots WHERE snapshots.region = samples.region
                                  AND snapshots.realm_id = samples.realm_id AND snapshots.observed_at = samples.observed_at)
              ORDER BY observed_at",
        )
        .bind(region.as_str()).bind(since.get() as i64).bind(item.get() as i64)
        .bind(region.as_str()).bind(since.get() as i64).bind(item.get() as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn last_observed(&self, region: Region, realm: RealmId) -> RepoResult<Option<Millis>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT MAX(observed_at) FROM (
                 SELECT observed_at FROM collection_snapshots WHERE region = ? AND realm_id = ?
                 UNION ALL
                 SELECT observed_at FROM realm_price_samples WHERE region = ? AND realm_id = ?
             )",
        )
        .bind(region.as_str())
        .bind(realm.get() as i64)
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
                 (item_id, region, realm_id, variant_id, observed_at,
                  min_price, median_price, max_price, listings)
             SELECT item_id, region, realm_id, variant_id,
                    (observed_at / 86400000) * 86400000,
                    MIN(min_price),
                    CAST(AVG(median_price) AS INTEGER),
                    MAX(max_price),
                    CAST(AVG(listings) AS INTEGER)
               FROM realm_price_samples
              WHERE observed_at < ?
              GROUP BY item_id, region, realm_id, variant_id, (observed_at / 86400000)
             ON CONFLICT(item_id, region, realm_id, variant_id, observed_at) DO UPDATE SET
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
            "INSERT INTO realms (realm_id, region, name, members, locale, enabled)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(realm_id, region) DO UPDATE SET
                    name = excluded.name,
                    members = excluded.members,
                    locale = excluded.locale",
        )
        .bind(realm.id.get() as i64)
        .bind(realm.region.as_str())
        .bind(&realm.name)
        .bind(serde_json::to_string(&realm.members).unwrap_or_else(|_| "[]".into()))
        .bind(&realm.locale)
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
        let rows = sqlx::query(
            "SELECT realm_id, region, name, members, locale, enabled
               FROM realms ORDER BY region, name",
        )
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
                    // A realm recorded before this column existed has none
                    // until the next startup; the joined name still shows.
                    members: serde_json::from_str(&row.get::<String, _>("members"))
                        .unwrap_or_default(),
                    locale: row.get("locale"),
                    enabled: row.get::<i64, _>("enabled") != 0,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use crate::{SqliteConfig, SqliteStore};
    use app_core::market::Listing;
    use app_core::repo::Store;

    #[tokio::test]
    async fn a_realm_ladder_failure_cannot_advance_last_observed() {
        let store = SqliteStore::connect(&SqliteConfig::in_memory())
            .await
            .unwrap();
        let prices = store.realm_prices();
        sqlx::query(
            "CREATE TRIGGER reject_test_realm_ladder BEFORE INSERT ON realm_price_ladders
             BEGIN SELECT RAISE(ABORT, 'injected ladder failure'); END",
        )
        .execute(&prices.pool)
        .await
        .unwrap();
        let at = Millis(1_000);
        let sample = RealmSample {
            item: ItemId(1),
            region: Region::Eu,
            realm: RealmId(1),
            variant: "plain".into(),
            observed_at: at,
            min_price: Copper(10),
            median_price: Copper(10),
            max_price: Copper(10),
            listings: 1,
        };
        let ladder = Ladder::of(&[Listing {
            item: ItemId(1),
            unit_price: Copper(10),
            quantity: 1,
        }]);
        assert!(
            prices
                .record_snapshot(
                    &[sample],
                    Region::Eu,
                    RealmId(1),
                    at,
                    &[(ItemId(1), "plain".into(), ladder)],
                )
                .await
                .is_err()
        );
        assert_eq!(
            prices.last_observed(Region::Eu, RealmId(1)).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_empty_realm_snapshot_is_still_an_observation() {
        let store = SqliteStore::connect(&SqliteConfig::in_memory())
            .await
            .unwrap();
        let prices = store.realm_prices();
        let at = Millis(1_000);

        prices
            .record_snapshot(&[], Region::Eu, RealmId(1), at, &[])
            .await
            .unwrap();

        assert_eq!(
            prices.last_observed(Region::Eu, RealmId(1)).await.unwrap(),
            Some(at)
        );
        let observations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM collection_snapshots
              WHERE region = 'eu' AND realm_id = 1 AND observed_at = 1000",
        )
        .fetch_one(&prices.pool)
        .await
        .unwrap();
        assert_eq!(observations, 1);
    }

    #[tokio::test]
    async fn ledger_expansion_keeps_unsuppressed_window_rows_identical() {
        let store = SqliteStore::connect(&SqliteConfig::in_memory())
            .await
            .unwrap();
        let prices = store.realm_prices();
        for (at, price) in [(Millis(1_000), 10), (Millis(2_000), 20)] {
            prices
                .record_snapshot(
                    &[RealmSample {
                        item: ItemId(1),
                        region: Region::Eu,
                        realm: RealmId(1),
                        variant: "plain".into(),
                        observed_at: at,
                        min_price: Copper(price),
                        median_price: Copper(price),
                        max_price: Copper(price),
                        listings: 1,
                    }],
                    Region::Eu,
                    RealmId(1),
                    at,
                    &[],
                )
                .await
                .unwrap();
        }
        let window = prices
            .window_in_region(Region::Eu, Millis::ZERO)
            .await
            .unwrap();
        assert_eq!(
            window
                .iter()
                .map(|sample| (sample.observed_at, sample.min_price))
                .collect::<Vec<_>>(),
            vec![(Millis(1_000), Copper(10)), (Millis(2_000), Copper(20))]
        );
        let history = prices
            .history(ItemId(1), Region::Eu, RealmId(1), Millis::ZERO)
            .await
            .unwrap();
        assert_eq!(
            history
                .iter()
                .map(|sample| (sample.observed_at, sample.min_price))
                .collect::<Vec<_>>(),
            vec![(Millis(1_000), Copper(10)), (Millis(2_000), Copper(20))]
        );
    }
}
