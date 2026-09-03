//! SQLite storage for TradeSkillMaster source data and its internal contrast.

use app_core::error::RepoResult;
use app_core::market::{ItemId, Region, TsmCommoditySample, TsmContrast, TsmRegionDaily};
use app_core::repo::TsmRepository;
use cluster_core::Millis;
use sqlx::{Pool, Row, Sqlite};

use super::map_err;

#[derive(Clone)]
pub struct SqliteTsm {
    pool: Pool<Sqlite>,
}

impl SqliteTsm {
    pub(crate) fn new(pool: Pool<Sqlite>) -> Self {
        Self { pool }
    }
}

impl TsmRepository for SqliteTsm {
    async fn record_region_daily(&self, samples: &[TsmRegionDaily]) -> RepoResult<u64> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0;
        for sample in samples {
            written += sqlx::query(
                "INSERT INTO tsm_region_daily
                   (item_id, region, day, market_value, historical, avg_sale_price,
                    sale_rate_bp, sold_per_day, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(item_id, region, day) DO UPDATE SET
                    market_value = excluded.market_value,
                    historical = excluded.historical,
                    avg_sale_price = excluded.avg_sale_price,
                    sale_rate_bp = excluded.sale_rate_bp,
                    sold_per_day = excluded.sold_per_day,
                    updated_at = excluded.updated_at",
            )
            .bind(sample.item.get() as i64)
            .bind(sample.region.as_str())
            .bind(sample.day.get() as i64)
            .bind(sample.market_value.get() as i64)
            .bind(sample.historical.get() as i64)
            .bind(sample.avg_sale_price.get() as i64)
            .bind(sample.sale_rate_bp as i64)
            .bind(sample.sold_per_day as i64)
            .bind(sample.updated_at.get() as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?
            .rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn record_commodity_samples(&self, samples: &[TsmCommoditySample]) -> RepoResult<u64> {
        let mut tx = self.pool.begin().await.map_err(map_err)?;
        let mut written = 0;
        for sample in samples {
            written += sqlx::query(
                "INSERT INTO tsm_commodity_sample
                   (item_id, region, observed_at, market_value, min_buyout, recent,
                    historical, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(item_id, region, observed_at) DO NOTHING",
            )
            .bind(sample.item.get() as i64)
            .bind(sample.region.as_str())
            .bind(sample.observed_at.get() as i64)
            .bind(sample.market_value.get() as i64)
            .bind(sample.min_buyout.get() as i64)
            .bind(sample.recent.get() as i64)
            .bind(sample.historical.get() as i64)
            .bind(sample.updated_at.get() as i64)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?
            .rows_affected();
        }
        tx.commit().await.map_err(map_err)?;
        Ok(written)
    }

    async fn has_commodity_snapshot(
        &self,
        region: Region,
        observed_at: Millis,
    ) -> RepoResult<bool> {
        let present = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                 SELECT 1 FROM tsm_commodity_sample WHERE region = ? AND observed_at = ?
             )",
        )
        .bind(region.as_str())
        .bind(observed_at.get() as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(present != 0)
    }

    async fn contrast(&self, region: Region, observed_at: Millis) -> RepoResult<Vec<TsmContrast>> {
        const ALIGNMENT_MS: u64 = 90 * 60 * 1_000;
        let before = observed_at.get().saturating_sub(ALIGNMENT_MS) as i64;
        let after = observed_at.plus_ms(ALIGNMENT_MS).get() as i64;
        // There is one TSM updatedAt per file. The local window comes back in
        // one query, then is grouped in memory, avoiding one query per item.
        let rows = sqlx::query(
            "SELECT t.item_id, t.min_buyout, t.market_value,
                    p.observed_at AS own_observed_at, p.min_unit, p.median_unit
               FROM tsm_commodity_sample t
               JOIN price_samples p
                 ON p.item_id = t.item_id AND p.region = t.region
              WHERE t.region = ? AND t.observed_at = ?
                AND p.observed_at BETWEEN ? AND ?
              ORDER BY t.item_id, p.observed_at",
        )
        .bind(region.as_str())
        .bind(observed_at.get() as i64)
        .bind(before)
        .bind(after)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;

        let mut out = Vec::new();
        let mut cursor = 0;
        while cursor < rows.len() {
            let item = rows[cursor].get::<i64, _>("item_id") as u32;
            let tsm_min = rows[cursor].get::<i64, _>("min_buyout") as u64;
            let tsm_value = rows[cursor].get::<i64, _>("market_value") as u64;
            let start = cursor;
            while cursor < rows.len() && rows[cursor].get::<i64, _>("item_id") as u32 == item {
                cursor += 1;
            }
            let group = &rows[start..cursor];
            // A single local sample cannot establish stability. Require one
            // on each side of TSM's scan time, then reject the whole window
            // when its minimum moved at all.
            let stable = group.len() >= 2
                && group
                    .iter()
                    .any(|row| row.get::<i64, _>("own_observed_at") < observed_at.get() as i64)
                && group
                    .iter()
                    .any(|row| row.get::<i64, _>("own_observed_at") > observed_at.get() as i64)
                && group.first().is_some_and(|first| {
                    group
                        .iter()
                        .all(|row| row.get::<i64, _>("min_unit") == first.get::<i64, _>("min_unit"))
                });
            if !stable {
                continue;
            }
            let nearest = group.iter().min_by_key(|row| {
                (row.get::<i64, _>("own_observed_at") as u64).abs_diff(observed_at.get())
            });
            if let Some(own) = nearest {
                let own_min = own.get::<i64, _>("min_unit") as u64;
                let own_median = own.get::<i64, _>("median_unit") as u64;
                out.push(TsmContrast {
                    item: ItemId(item),
                    region,
                    observed_at,
                    own_observed_at: Millis(own.get::<i64, _>("own_observed_at") as u64),
                    min_buyout_matches: tsm_min == own_min,
                    market_value_ratio_bp: (own_median != 0).then(|| {
                        tsm_value.saturating_mul(10_000).saturating_div(own_median) as u32
                    }),
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::market::{Copper, PriceSample};
    use app_core::repo::{PriceRepository, Store};

    #[tokio::test]
    async fn contrast_discards_moving_markets_and_compares_stable_ones() {
        let store = super::super::SqliteStore::connect(&super::super::SqliteConfig::in_memory())
            .await
            .unwrap();
        let at = Millis(1_724_133_600_000);
        store
            .prices()
            .record_samples(&[
                PriceSample {
                    item: ItemId(1),
                    region: Region::Eu,
                    observed_at: Millis(at.get() - 30 * 60 * 1_000),
                    min_unit_price: Copper(100),
                    p05_unit_price: Copper(100),
                    median_unit_price: Copper(110),
                    quantity: 1,
                    listings: 1,
                },
                PriceSample {
                    item: ItemId(1),
                    region: Region::Eu,
                    observed_at: at.plus_ms(60 * 60 * 1_000),
                    min_unit_price: Copper(100),
                    p05_unit_price: Copper(100),
                    median_unit_price: Copper(110),
                    quantity: 1,
                    listings: 1,
                },
                PriceSample {
                    item: ItemId(2),
                    region: Region::Eu,
                    observed_at: Millis(at.get() - 30 * 60 * 1_000),
                    min_unit_price: Copper(100),
                    p05_unit_price: Copper(100),
                    median_unit_price: Copper(100),
                    quantity: 1,
                    listings: 1,
                },
                PriceSample {
                    item: ItemId(2),
                    region: Region::Eu,
                    observed_at: at.plus_ms(60 * 60 * 1_000),
                    min_unit_price: Copper(101),
                    p05_unit_price: Copper(101),
                    median_unit_price: Copper(101),
                    quantity: 1,
                    listings: 1,
                },
            ])
            .await
            .unwrap();
        store
            .tsm()
            .record_commodity_samples(&[
                TsmCommoditySample {
                    item: ItemId(1),
                    region: Region::Eu,
                    observed_at: at,
                    market_value: Copper(121),
                    min_buyout: Copper(100),
                    recent: Copper(0),
                    historical: Copper(0),
                    updated_at: at,
                },
                TsmCommoditySample {
                    item: ItemId(2),
                    region: Region::Eu,
                    observed_at: at,
                    market_value: Copper(100),
                    min_buyout: Copper(100),
                    recent: Copper(0),
                    historical: Copper(0),
                    updated_at: at,
                },
            ])
            .await
            .unwrap();

        let contrast = store.tsm().contrast(Region::Eu, at).await.unwrap();
        assert_eq!(contrast.len(), 1);
        assert!(contrast[0].min_buyout_matches);
        assert_eq!(contrast[0].market_value_ratio_bp, Some(11_000));
    }
}
