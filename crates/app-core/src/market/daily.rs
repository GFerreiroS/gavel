use crate::market::engine::{Buckets, Distribution, Gates};
use cluster_core::Millis;

#[derive(Debug, Clone, Default)]
pub struct DailyRollup {
    pub open_price: u64,
    pub close_price: u64,
    pub low_price: u64,
    pub low_at: Millis,
    pub high_price: u64,
    pub high_at: Millis,
    pub mean_price: u64,
    pub p05_price: u64,
    pub p25_price: u64,
    pub median_price: u64,
    pub p75_price: u64,
    pub p95_price: u64,
    pub open_quantity: u64,
    pub close_quantity: u64,
    pub mean_quantity: u64,
    pub open_listings: u64,
    pub close_listings: u64,
    pub mean_listings: u64,
    pub samples: u32,
    pub observed_buckets: u32,
    pub insufficient: Option<String>,
    pub insufficient_have: Option<u32>,
    pub insufficient_need: Option<u32>,
}

use crate::market::Copper;

impl DailyRollup {
    pub fn compute(observations: &[(Millis, u64, u64, u64)]) -> Self {
        if observations.is_empty() {
            return DailyRollup {
                insufficient: Some("No observations".into()),
                ..Default::default()
            };
        }

        let mut min_price = u64::MAX;
        let mut min_at = Millis(0);
        let mut max_price = 0;
        let mut max_at = Millis(0);

        let mut samples_count = 0;

        for &(at, price, _, listings) in observations {
            if listings > 0 {
                samples_count += 1;
                if price < min_price {
                    min_price = price;
                    min_at = at;
                }
                if price > max_price {
                    max_price = price;
                    max_at = at;
                }
            }
        }

        // We use Buckets for duration-weighting.
        let price_buckets = Buckets::from_observations(
            observations
                .iter()
                .filter(|&&(_, _, _, listings)| listings > 0)
                .map(|&(at, price, _, _)| (at, Copper(price))),
        );

        // For quantity and listings we can group by hour manually:
        let mut hourly_q: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
        let mut hourly_l: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();

        for &(at, _, quantity, listings) in observations {
            if listings > 0 {
                let bucket = at.get() / crate::market::engine::BUCKET_MS;
                hourly_q.insert(bucket, quantity);
                hourly_l.insert(bucket, listings);
            }
        }

        let q_vals: Vec<u64> = hourly_q.values().copied().collect();
        let l_vals: Vec<u64> = hourly_l.values().copied().collect();

        let mut chronological_prices: std::collections::BTreeMap<u64, u64> =
            std::collections::BTreeMap::new();
        for &(at, price, _, listings) in observations {
            if listings > 0 {
                let bucket = at.get() / crate::market::engine::BUCKET_MS;
                chronological_prices.insert(bucket, price);
            }
        }
        let p_chronological: Vec<u64> = chronological_prices.values().copied().collect();

        if p_chronological.is_empty() {
            return DailyRollup {
                insufficient: Some("No active listings".into()),
                ..Default::default()
            };
        }

        let mut rollup = DailyRollup {
            samples: samples_count,
            observed_buckets: p_chronological.len() as u32,
            ..Default::default()
        };

        let gates = Gates {
            median: 8, // A day has 24 hours. We need some reasonable gates.
            tails: 12,
            coverage: 25,
        };

        use crate::market::engine::Insufficient;
        if let Err(e) = gates.admit(
            rollup.observed_buckets,
            Some((rollup.observed_buckets * 100) / 24),
        ) {
            rollup.insufficient = Some(match e {
                Insufficient::NotEnoughHistory { .. } => "not enough history".into(),
                Insufficient::TooManyGaps { .. } => "too many gaps".into(),
            });
            rollup.insufficient_have = Some(rollup.observed_buckets);
            rollup.insufficient_need = Some(gates.median); // Just something
            // Do NOT render a confident daily value. We leave the stats at 0.
            return rollup;
        }

        rollup.open_price = *p_chronological.first().unwrap();
        rollup.close_price = *p_chronological.last().unwrap();
        rollup.low_price = min_price;
        rollup.low_at = min_at;
        rollup.high_price = max_price;
        rollup.high_at = max_at;
        rollup.mean_price =
            (p_chronological.iter().sum::<u64>() as f64 / p_chronological.len() as f64) as u64;

        if let Some(dist) = Distribution::of(&price_buckets) {
            rollup.p05_price = dist.p05.0;
            rollup.p25_price = dist.p25.0;
            rollup.median_price = dist.median.0;
            rollup.p75_price = dist.p75.0;
            rollup.p95_price = dist.p95.0;
        }

        rollup.open_quantity = *q_vals.first().unwrap();
        rollup.close_quantity = *q_vals.last().unwrap();
        rollup.mean_quantity = (q_vals.iter().sum::<u64>() as f64 / q_vals.len() as f64) as u64;

        rollup.open_listings = *l_vals.first().unwrap();
        rollup.close_listings = *l_vals.last().unwrap();
        rollup.mean_listings = (l_vals.iter().sum::<u64>() as f64 / l_vals.len() as f64) as u64;

        rollup
    }
}
