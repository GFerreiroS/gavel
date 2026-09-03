use app_core::error::RepoResult;
use app_core::market::{Alert, AlertSeverity, Copper, ItemId, Ladder, PriceSample, Region};
use app_core::repo::PriceRepository;
use cluster_core::Millis;
use sqlx::sqlite::SqliteRow;
use sqlx::{Pool, Row, Sqlite};

use super::{corrupt, map_err};

#[derive(Clone)]
pub struct SqlitePrices {
    pub(super) pool: Pool<Sqlite>,
}

impl SqlitePrices {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

fn sample_from_row(row: &SqliteRow) -> RepoResult<PriceSample> {
    let region: String = row.get("region");
    Ok(PriceSample {
        item: ItemId(row.get::<i64, _>("item_id") as u32),
        region: Region::parse(&region).ok_or_else(|| corrupt("region", region))?,
        observed_at: Millis(row.get::<i64, _>("observed_at") as u64),
        min_unit_price: Copper(row.get::<i64, _>("min_unit") as u64),
        p05_unit_price: Copper(row.get::<i64, _>("p05_unit") as u64),
        median_unit_price: Copper(row.get::<i64, _>("median_unit") as u64),
        quantity: row.get::<i64, _>("quantity") as u64,
        listings: row.get::<i64, _>("listings") as u32,
    })
}

impl PriceRepository for SqlitePrices {
    async fn record_snapshot(
        &self,
        samples: &[PriceSample],
        region: Region,
        observed_at: Millis,
        ladders: &[(ItemId, Ladder)],
    ) -> RepoResult<(u64, u64)> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut sample_rows = 0u64;
        for sample in samples {
            sample_rows += sqlx::query(
                "INSERT INTO price_samples
                   (item_id, region, observed_at, min_unit, p05_unit, median_unit, quantity, listings)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(item_id, region, observed_at) DO NOTHING",
            )
            .bind(sample.item.get() as i64)
            .bind(sample.region.as_str())
            .bind(sample.observed_at.get() as i64)
            .bind(sample.min_unit_price.get() as i64)
            .bind(sample.p05_unit_price.get() as i64)
            .bind(sample.median_unit_price.get() as i64)
            .bind(sample.quantity as i64)
            .bind(sample.listings as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?
            .rows_affected();
        }
        let mut ladder_rows = 0u64;
        for (item, ladder) in ladders.iter().filter(|(_, ladder)| !ladder.is_empty()) {
            ladder_rows += sqlx::query(
                "INSERT OR IGNORE INTO price_ladders
                   (item_id, region, observed_at, levels, total, steps)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(item.get() as i64)
            .bind(region.as_str())
            .bind(observed_at.get() as i64)
            .bind(ladder.levels() as i64)
            .bind(ladder.total() as i64)
            .bind(ladder.encode())
            .execute(&mut *tx)
            .await
            .map_err(map_err)?
            .rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok((sample_rows, ladder_rows))
    }

    /// One transaction for the whole snapshot: either the hour lands or it
    /// does not, so a partial write cannot skew a baseline.
    async fn record_samples(&self, samples: &[PriceSample]) -> RepoResult<u64> {
        if samples.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0u64;
        for sample in samples {
            let result = sqlx::query(
                "INSERT INTO price_samples
                   (item_id, region, observed_at, min_unit, p05_unit, median_unit, quantity, listings)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(item_id, region, observed_at) DO NOTHING",
            )
            .bind(sample.item.get() as i64)
            .bind(sample.region.as_str())
            .bind(sample.observed_at.get() as i64)
            .bind(sample.min_unit_price.get() as i64)
            .bind(sample.p05_unit_price.get() as i64)
            .bind(sample.median_unit_price.get() as i64)
            .bind(sample.quantity as i64)
            .bind(sample.listings as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            written += result.rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn history(
        &self,
        item: ItemId,
        region: Region,
        since: Millis,
    ) -> RepoResult<Vec<PriceSample>> {
        let rows = sqlx::query(
            "SELECT * FROM price_samples
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

    async fn history_in_region(&self, region: Region) -> RepoResult<Vec<PriceSample>> {
        // Ordered by market and then by time, which is the order the
        // materialiser wants to group in and the order the index already
        // holds them in.
        let rows = sqlx::query(
            "SELECT * FROM price_samples
              WHERE region = ?
              ORDER BY item_id, observed_at",
        )
        .bind(region.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn latest(&self, region: Region) -> RepoResult<Vec<PriceSample>> {
        // One row per item: the newest observation we hold.
        let rows = sqlx::query(
            "SELECT s.* FROM price_samples s
             JOIN (SELECT item_id, MAX(observed_at) AS newest
                     FROM price_samples WHERE region = ?
                    GROUP BY item_id) latest
               ON s.item_id = latest.item_id AND s.observed_at = latest.newest
             WHERE s.region = ?
             ORDER BY s.item_id",
        )
        .bind(region.as_str())
        .bind(region.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(sample_from_row).collect()
    }

    async fn last_observed(&self, region: Region) -> RepoResult<Option<Millis>> {
        let row =
            sqlx::query("SELECT MAX(observed_at) AS newest FROM price_samples WHERE region = ?")
                .bind(region.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(map_err)?;
        Ok(row
            .get::<Option<i64>, _>("newest")
            .map(|v| Millis(v as u64)))
    }

    async fn record_alert(&self, alert: &Alert) -> RepoResult<()> {
        sqlx::query(
            "INSERT INTO price_alerts
               (item_id, region, severity, observed_at, current_c, baseline_c, threshold_c, discount, quantity)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(alert.item.get() as i64)
        .bind(alert.region.as_str())
        .bind(alert.severity.as_str())
        .bind(alert.observed_at.get() as i64)
        .bind(alert.current.get() as i64)
        .bind(alert.baseline.get() as i64)
        .bind(alert.threshold.get() as i64)
        .bind(alert.discount_percent as i64)
        .bind(alert.quantity as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    async fn last_alert_at(&self, item: ItemId, region: Region) -> RepoResult<Option<Millis>> {
        let row = sqlx::query(
            "SELECT MAX(observed_at) AS newest FROM price_alerts WHERE item_id = ? AND region = ?",
        )
        .bind(item.get() as i64)
        .bind(region.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(row
            .get::<Option<i64>, _>("newest")
            .map(|v| Millis(v as u64)))
    }

    async fn recent_alerts(&self, limit: usize) -> RepoResult<Vec<Alert>> {
        let rows =
            sqlx::query("SELECT * FROM price_alerts ORDER BY observed_at DESC, id DESC LIMIT ?")
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(map_err)?;
        rows.iter().map(alert_from_row).collect()
    }

    async fn alerts_since(&self, since: Millis, limit: usize) -> RepoResult<Vec<Alert>> {
        // `idx_alerts_time` is on `observed_at DESC`, so this is a range scan
        // from the cut-off rather than a sort of the whole table.
        let rows = sqlx::query(
            "SELECT item_id, region, severity, observed_at,
                    current_c, baseline_c, threshold_c, discount, quantity
               FROM price_alerts
              WHERE observed_at >= ?
              ORDER BY observed_at DESC
              LIMIT ?",
        )
        .bind(since.get() as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(alert_from_row).collect()
    }

    async fn window_stats(
        &self,
        region: Region,
        since: Millis,
        until: Option<Millis>,
    ) -> RepoResult<Vec<app_core::market::WindowStats>> {
        self.window_stats_inner(region, since, until).await
    }

    /// One day of a market becomes one row: the day's cheapest price, and the
    /// average of everything else.
    ///
    /// `min_unit` stays a true minimum, because "how cheap did this get" is
    /// the question the archive is kept for. The percentile and the depth
    /// become the day's average, which is what a single row can honestly say
    /// about twenty-four of them.
    ///
    /// Idempotent: a second run finds nothing left to collapse, because the
    /// rows it wrote sit exactly on midnight and every other row older than
    /// the cutoff has gone.
    async fn downsample_before(&self, before: Millis) -> RepoResult<u64> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        // Written first, so a failure between the two statements leaves the
        // full-resolution rows in place rather than losing the day.
        sqlx::query(
            "INSERT INTO price_samples
                 (item_id, region, observed_at, min_unit, p05_unit, median_unit,
                  quantity, listings)
             SELECT item_id, region, (observed_at / 86400000) * 86400000,
                    MIN(min_unit),
                    CAST(AVG(p05_unit) AS INTEGER),
                    CAST(AVG(median_unit) AS INTEGER),
                    CAST(AVG(quantity) AS INTEGER),
                    CAST(AVG(listings) AS INTEGER)
               FROM price_samples
              WHERE observed_at < ?
              GROUP BY item_id, region, (observed_at / 86400000)
             ON CONFLICT(item_id, region, observed_at) DO UPDATE SET
                    min_unit    = excluded.min_unit,
                    p05_unit    = excluded.p05_unit,
                    median_unit = excluded.median_unit,
                    quantity    = excluded.quantity,
                    listings    = excluded.listings",
        )
        .bind(before.get() as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        let removed = sqlx::query(
            "DELETE FROM price_samples
              WHERE observed_at < ? AND observed_at % 86400000 != 0",
        )
        .bind(before.get() as i64)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;

        tx.commit().await.map_err(map_err)?;
        Ok(removed.rows_affected())
    }

    async fn prune_before(&self, before: Millis) -> RepoResult<u64> {
        let result = sqlx::query("DELETE FROM price_samples WHERE observed_at < ?")
            .bind(before.get() as i64)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(result.rows_affected())
    }

    async fn record_ladders(
        &self,
        region: Region,
        observed_at: Millis,
        ladders: &[(ItemId, Ladder)],
    ) -> RepoResult<u64> {
        if ladders.is_empty() {
            return Ok(0);
        }
        // One transaction for the whole snapshot, like `record_samples`: two
        // thousand ladders is two thousand statements, and a commit apiece
        // would make a collection cycle a disk-sync storm.
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0;
        for (item, ladder) in ladders {
            if ladder.is_empty() {
                continue;
            }
            // `OR IGNORE`, not upsert: an instant that is already recorded is
            // the same snapshot, and rewriting it would let a retry replace a
            // ladder with a differently-parsed copy of itself.
            let result = sqlx::query(
                "INSERT OR IGNORE INTO price_ladders
                   (item_id, region, observed_at, levels, total, steps)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(item.get() as i64)
            .bind(region.as_str())
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

    async fn latest_ladders(&self, region: Region) -> RepoResult<Vec<(ItemId, Millis, Ladder)>> {
        // The bare-column form of "the newest row per item", which §11b
        // records as 4.5x faster than the window function on the real archive
        // and which `latest_matches_the_window_function` holds against the
        // portable spelling.
        let rows = sqlx::query(
            "SELECT item_id, max(observed_at) AS observed_at, steps
               FROM price_ladders
              WHERE region = ?
              GROUP BY item_id",
        )
        .bind(region.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    ItemId(row.get::<i64, _>("item_id") as u32),
                    Millis(row.get::<i64, _>("observed_at") as u64),
                    Ladder::decode(&row.get::<String, _>("steps")),
                )
            })
            .collect())
    }

    async fn prune_ladders_before(&self, before: Millis) -> RepoResult<u64> {
        let result = sqlx::query("DELETE FROM price_ladders WHERE observed_at < ?")
            .bind(before.get() as i64)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(result.rows_affected())
    }
}

impl SqlitePrices {
    /// Separate from the trait impl only because `WindowStats` is a UI concern
    /// and this is the one query that aggregates rather than returning rows.
    async fn window_stats_inner(
        &self,
        region: Region,
        since: Millis,
        until: Option<Millis>,
    ) -> RepoResult<Vec<app_core::market::WindowStats>> {
        // Half-open: a patch boundary belongs to the patch that starts on it,
        // so consecutive windows cannot double-count an hour.
        //
        // `observed_at` is a bare column beside MIN()/MAX(): SQLite defines
        // that to yield the value from the row the extreme came from, which is
        // exactly the "when was it cheapest" the cards show. Two passes,
        // because one query can only anchor bare columns to one extreme.
        let end = until.map(|u| u.get() as i64).unwrap_or(i64::MAX);

        let lows = sqlx::query(
            "SELECT item_id, MIN(p05_unit) AS low, observed_at AS low_at
               FROM price_samples
              WHERE region = ? AND observed_at >= ? AND observed_at < ?
              GROUP BY item_id",
        )
        .bind(region.as_str())
        .bind(since.get() as i64)
        .bind(end)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;

        let low_by_item: std::collections::BTreeMap<i64, (i64, i64)> = lows
            .into_iter()
            .map(|row| {
                (
                    row.get::<i64, _>("item_id"),
                    (row.get::<i64, _>("low"), row.get::<i64, _>("low_at")),
                )
            })
            .collect();

        let rows = sqlx::query(
            "SELECT item_id,
                    MAX(p05_unit) AS high,
                    observed_at   AS high_at,
                    AVG(p05_unit) AS mean,
                    COUNT(*)      AS samples
               FROM price_samples
              WHERE region = ? AND observed_at >= ? AND observed_at < ?
              GROUP BY item_id
              ORDER BY item_id",
        )
        .bind(region.as_str())
        .bind(since.get() as i64)
        .bind(end)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let id = row.get::<i64, _>("item_id");
                let (low, low_at) = low_by_item.get(&id).copied().unwrap_or((0, 0));
                app_core::market::WindowStats {
                    item: ItemId(id as u32),
                    low: Copper(low as u64),
                    low_at: Millis(low_at as u64),
                    high: Copper(row.get::<i64, _>("high") as u64),
                    high_at: Millis(row.get::<i64, _>("high_at") as u64),
                    mean: Copper(row.get::<f64, _>("mean") as u64),
                    samples: row.get::<i64, _>("samples") as u32,
                }
            })
            .collect())
    }
}

/// One `price_alerts` row as a domain [`Alert`].
///
/// Shared by both reads, so a column added to one cannot be forgotten in the
/// other -- which is how the two would drift.
fn alert_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<Alert> {
    let region: String = row.get("region");
    let severity: String = row.get("severity");
    Ok(Alert {
        item: ItemId(row.get::<i64, _>("item_id") as u32),
        region: Region::parse(&region).ok_or_else(|| corrupt("region", region))?,
        severity: match severity.as_str() {
            "very_low" => AlertSeverity::VeryLow,
            "low" => AlertSeverity::Low,
            other => return Err(corrupt("alert severity", other)),
        },
        observed_at: Millis(row.get::<i64, _>("observed_at") as u64),
        current: Copper(row.get::<i64, _>("current_c") as u64),
        baseline: Copper(row.get::<i64, _>("baseline_c") as u64),
        threshold: Copper(row.get::<i64, _>("threshold_c") as u64),
        discount_percent: row.get::<i64, _>("discount") as u8,
        quantity: row.get::<i64, _>("quantity") as u64,
    })
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use crate::{SqliteConfig, SqliteStore};
    use app_core::market::Listing;
    use app_core::repo::Store;

    #[tokio::test]
    async fn a_ladder_failure_rolls_back_the_samples_from_the_same_snapshot() {
        let store = SqliteStore::connect(&SqliteConfig::in_memory())
            .await
            .unwrap();
        let prices = store.prices();
        sqlx::query(
            "CREATE TRIGGER reject_test_ladder BEFORE INSERT ON price_ladders
             BEGIN SELECT RAISE(ABORT, 'injected ladder failure'); END",
        )
        .execute(&prices.pool)
        .await
        .unwrap();
        let at = Millis(1_000);
        let sample = PriceSample {
            item: ItemId(1),
            region: Region::Eu,
            observed_at: at,
            min_unit_price: Copper(10),
            p05_unit_price: Copper(10),
            median_unit_price: Copper(10),
            quantity: 1,
            listings: 1,
        };
        let ladder = Ladder::of(&[Listing {
            item: ItemId(1),
            unit_price: Copper(10),
            quantity: 1,
        }]);
        assert!(
            prices
                .record_snapshot(&[sample], Region::Eu, at, &[(ItemId(1), ladder)])
                .await
                .is_err()
        );
        assert!(
            prices
                .history(ItemId(1), Region::Eu, Millis(0))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(prices.last_observed(Region::Eu).await.unwrap(), None);
    }
}
