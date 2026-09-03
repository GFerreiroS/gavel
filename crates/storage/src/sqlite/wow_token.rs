use app_core::error::RepoResult;
use app_core::market::{Copper, Region};
use app_core::repo::{TokenPriceRepository, WowTokenPrice};
use cluster_core::Millis;
use sqlx::Row;

use super::{corrupt, map_err, prices::SqlitePrices};

fn from_row(row: &sqlx::sqlite::SqliteRow) -> RepoResult<WowTokenPrice> {
    let region: String = row.get("region");
    Ok(WowTokenPrice {
        region: Region::parse(&region).ok_or_else(|| corrupt("region", region))?,
        observed_at: Millis(row.get::<i64, _>("observed_at") as u64),
        price: Copper(row.get::<i64, _>("price") as u64),
    })
}

impl TokenPriceRepository for SqlitePrices {
    async fn record(&self, token: &WowTokenPrice) -> RepoResult<bool> {
        let result = sqlx::query(
            "INSERT INTO wow_token_prices (region, observed_at, price)
             VALUES (?, ?, ?)
             ON CONFLICT(region, observed_at) DO NOTHING",
        )
        .bind(token.region.as_str())
        .bind(token.observed_at.get() as i64)
        .bind(token.price.get() as i64)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(result.rows_affected() == 1)
    }

    async fn history(&self, region: Region) -> RepoResult<Vec<WowTokenPrice>> {
        let rows = sqlx::query(
            "SELECT region, observed_at, price
             FROM wow_token_prices
             WHERE region = ?
             ORDER BY observed_at",
        )
        .bind(region.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        rows.iter().map(from_row).collect()
    }
}

#[cfg(test)]
mod tests {
    use app_core::repo::{Store, TokenPriceRepository};

    use super::*;
    use crate::{SqliteConfig, SqliteStore};

    #[tokio::test]
    async fn token_price_history_round_trips_and_is_idempotent() {
        let store = SqliteStore::connect(&SqliteConfig::in_memory())
            .await
            .unwrap();
        let first = WowTokenPrice {
            region: Region::Eu,
            observed_at: Millis(1_000),
            price: Copper(1_234_000),
        };
        let second = WowTokenPrice {
            observed_at: Millis(2_000),
            price: Copper(1_250_000),
            ..first
        };

        assert!(store.prices().record(&first).await.unwrap());
        assert!(!store.prices().record(&first).await.unwrap());
        assert!(store.prices().record(&second).await.unwrap());
        assert_eq!(
            store.prices().history(Region::Eu).await.unwrap(),
            vec![first, second]
        );
        assert!(store.prices().history(Region::Us).await.unwrap().is_empty());
    }
}
