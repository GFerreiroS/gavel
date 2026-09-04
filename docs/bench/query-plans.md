# Query plans

Recorded by `scripts/query-plans.py`. Regenerate it whenever an index,
a query or the statistics change, and say in the commit message what
moved -- CLAUDE.md §11b's rule is to check the plan, and a plan nobody
wrote down is a plan nobody can compare against.

Fixture: `target/bench/market-synthetic.db`
The deterministic synthetic fixture is generated on demand for this check; query-plan shape is reproducible, while latency remains a real-archive measurement.
`sqlite_stat1` present: **yes** -- the planner guesses without it, and guessed four times slower on every category page.

Two phrases are worth grepping for. `USE TEMP B-TREE FOR ORDER BY`
means the index did not deliver the order the query asked for.
`SCAN` without `USING INDEX` means the whole table was read.

## daily rollup commodity history

Loads the day's raw history for the commodity daily rollup..
`crates/storage/src/sqlite/prices.rs`

```sql
SELECT item_id, region, observed_at, min_unit, quantity, listings
   FROM price_samples
  WHERE observed_at >= ? AND observed_at < ?
  ORDER BY item_id, region, observed_at
```

```text
SEARCH price_samples USING INDEX idx_prices_time (ANY(region) AND observed_at>? AND observed_at<?)
USE TEMP B-TREE FOR ORDER BY
```

## daily rollup commodity insert

Persists the computed daily row for a commodity market..
`crates/storage/src/sqlite/prices.rs`

```sql
INSERT OR REPLACE INTO price_daily
     (item_id, region, day, open_price, close_price, low_price, low_at, high_price, high_at,
      mean_price, p05_price, p25_price, median_price, p75_price, p95_price,
      open_quantity, close_quantity, mean_quantity,
      open_listings, close_listings, mean_listings,
      samples, observed_buckets, insufficient, insufficient_have, insufficient_need)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
```

```text
```

## daily rollup realm history

Expands the day's change rows for the gear/recipe daily rollup..
`crates/storage/src/sqlite/realm_prices.rs`

```sql
WITH expanded AS (
     SELECT samples.item_id, samples.region, samples.realm_id, samples.variant_id,
            snapshots.observed_at, samples.min_price, samples.median_price, samples.max_price, samples.listings
       FROM collection_snapshots AS snapshots JOIN realm_price_samples AS samples
         ON samples.region = snapshots.region AND samples.realm_id = snapshots.realm_id
      WHERE snapshots.observed_at >= ? AND snapshots.observed_at < ?
        AND samples.observed_at = (SELECT MAX(previous.observed_at) FROM realm_price_samples AS previous
             WHERE previous.region = snapshots.region AND previous.realm_id = snapshots.realm_id
               AND previous.item_id = samples.item_id AND previous.variant_id = samples.variant_id
               AND previous.observed_at <= snapshots.observed_at)
 )
 SELECT expanded.item_id, expanded.region, expanded.realm_id, expanded.variant_id, expanded.observed_at,
        expanded.min_price, expanded.median_price, expanded.max_price, expanded.listings
   FROM expanded
  WHERE expanded.listings > 0
  ORDER BY expanded.item_id, expanded.region, expanded.realm_id, expanded.variant_id, expanded.observed_at
```

```text
SCAN samples
SEARCH snapshots USING PRIMARY KEY (region=? AND realm_id=? AND observed_at>? AND observed_at<?)
CORRELATED SCALAR SUBQUERY 1
  SEARCH previous USING PRIMARY KEY (item_id=? AND region=? AND realm_id=? AND variant_id=? AND observed_at<?)
USE TEMP B-TREE FOR RIGHT PART OF ORDER BY
```

## daily rollup realm insert

Persists the computed daily row for a gear/recipe market..
`crates/storage/src/sqlite/realm_prices.rs`

```sql
INSERT OR REPLACE INTO realm_price_daily
     (item_id, region, realm_id, variant_id, day, open_price, close_price, low_price, low_at, high_price, high_at,
      mean_price, p05_price, p25_price, median_price, p75_price, p95_price,
      open_listings, close_listings, mean_listings,
      samples, observed_buckets, insufficient, insufficient_have, insufficient_need)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
```

```text
```

## commodity latest, one region

every commodity category page: one row per market, newest first.
`crates/storage/src/sqlite/prices.rs`

```sql
SELECT s.* FROM price_samples s
 JOIN (SELECT item_id, MAX(observed_at) AS newest
         FROM price_samples WHERE region = ?
        GROUP BY item_id) latest
   ON s.item_id = latest.item_id AND s.observed_at = latest.newest
 WHERE s.region = ?
 ORDER BY s.item_id
```

```text
CO-ROUTINE latest
  SEARCH price_samples USING COVERING INDEX idx_prices_window (region=?)
SCAN latest
SEARCH s USING PRIMARY KEY (item_id=? AND region=? AND observed_at=?)
USE TEMP B-TREE FOR ORDER BY
```

## commodity window statistics

the card's comparison window, and the all-time extremes beside it.
`crates/storage/src/sqlite/prices.rs`

```sql
SELECT item_id,
       MAX(p05_unit) AS high,
       observed_at   AS high_at,
       AVG(p05_unit) AS mean,
       COUNT(*)      AS samples
  FROM price_samples
 WHERE region = ? AND observed_at >= ? AND observed_at < ?
 GROUP BY item_id
 ORDER BY item_id
```

```text
SEARCH price_samples USING INDEX idx_prices_time (region=? AND observed_at>? AND observed_at<?)
USE TEMP B-TREE FOR GROUP BY
```

## commodity history, one market

the analysis page's full-history reduction, which Phase 2 removes.
`crates/storage/src/sqlite/prices.rs`

```sql
SELECT * FROM price_samples
 WHERE item_id = ? AND region = ? AND observed_at >= ?
 ORDER BY observed_at
```

```text
SEARCH price_samples USING PRIMARY KEY (item_id=? AND region=? AND observed_at>?)
```

## per-realm latest, whole region

the gear and recipe pages: 18k markets rebuilt to draw nine cards.
`crates/storage/src/sqlite/realm_prices.rs`

```sql
SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
       MAX(samples.observed_at) AS observed_at, samples.min_price,
       samples.median_price, samples.max_price, samples.listings
  FROM realm_price_samples AS samples
  JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
 WHERE samples.region = ?
 GROUP BY samples.item_id, samples.realm_id, samples.variant_id
```

```text
SEARCH samples USING PRIMARY KEY (ANY(item_id) AND region=?)
SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
```

## per-realm latest, one realm

the same pages once a realm is chosen.
`crates/storage/src/sqlite/realm_prices.rs`

```sql
SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
       MAX(samples.observed_at) AS observed_at, samples.min_price,
       samples.median_price, samples.max_price, samples.listings
  FROM realm_price_samples AS samples
  JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
 WHERE samples.region = ? AND samples.realm_id = ?
 GROUP BY samples.item_id, samples.realm_id, samples.variant_id
```

```text
SEARCH samples USING PRIMARY KEY (ANY(item_id) AND region=? AND realm_id=?)
SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
```

## per-realm history, one item across a region

the BoE analysis page: one track on every realm of a region.
`crates/storage/src/sqlite/realm_prices.rs`

```sql
WITH expanded AS (
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
 UNION ALL
 SELECT samples.item_id, samples.region, samples.realm_id, variants.variant, samples.observed_at,
        samples.min_price, samples.median_price, samples.max_price, samples.listings
   FROM realm_price_samples AS samples JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
  WHERE samples.region = ? AND samples.observed_at >= ? AND samples.item_id = ?
    AND NOT EXISTS (SELECT 1 FROM collection_snapshots AS snapshots WHERE snapshots.region = samples.region
                      AND snapshots.realm_id = samples.realm_id AND snapshots.observed_at = samples.observed_at)
  ORDER BY observed_at
```

```text
MERGE (UNION ALL)
  LEFT
    SEARCH snapshots USING PRIMARY KEY (region=?)
    SEARCH samples USING PRIMARY KEY (item_id=? AND region=? AND realm_id=?)
    CORRELATED SCALAR SUBQUERY 1
      SEARCH previous USING PRIMARY KEY (item_id=? AND region=? AND realm_id=? AND variant_id=? AND observed_at<?)
    SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
    USE TEMP B-TREE FOR ORDER BY
  RIGHT
    SEARCH samples USING PRIMARY KEY (item_id=? AND region=?)
    CORRELATED SCALAR SUBQUERY 4
      SEARCH snapshots USING PRIMARY KEY (region=? AND realm_id=? AND observed_at=?)
    SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
    USE TEMP B-TREE FOR ORDER BY
```

## per-realm history, one item on one realm

the single-realm full history view.
`crates/storage/src/sqlite/realm_prices.rs`

```sql
WITH expanded AS (
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
 UNION ALL
 SELECT samples.item_id, samples.region, samples.realm_id, variants.variant, samples.observed_at,
        samples.min_price, samples.median_price, samples.max_price, samples.listings
   FROM realm_price_samples AS samples JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
  WHERE samples.region = ? AND samples.observed_at >= ? AND samples.item_id = ? AND samples.realm_id = ?
    AND NOT EXISTS (SELECT 1 FROM collection_snapshots AS snapshots WHERE snapshots.region = samples.region
                      AND snapshots.realm_id = samples.realm_id AND snapshots.observed_at = samples.observed_at)
  ORDER BY observed_at
```

```text
MERGE (UNION ALL)
  LEFT
    SEARCH snapshots USING PRIMARY KEY (region=? AND realm_id=? AND observed_at>?)
    SEARCH samples USING PRIMARY KEY (item_id=? AND region=? AND realm_id=?)
    CORRELATED SCALAR SUBQUERY 1
      SEARCH previous USING PRIMARY KEY (item_id=? AND region=? AND realm_id=? AND variant_id=? AND observed_at<?)
    SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
  RIGHT
    SEARCH samples USING INDEX idx_realm_prices_item (item_id=? AND region=? AND realm_id=? AND observed_at>?)
    CORRELATED SCALAR SUBQUERY 4
      SEARCH snapshots USING PRIMARY KEY (region=? AND realm_id=? AND observed_at=?)
    SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
```

## per-realm window, whole region

the background materialiser expanding ledger evidence into a window.
`crates/storage/src/sqlite/realm_prices.rs`

```sql
WITH expanded AS (
     SELECT samples.item_id, samples.region, samples.realm_id, samples.variant_id,
            snapshots.observed_at, samples.min_price, samples.median_price,
            samples.max_price, samples.listings
       FROM collection_snapshots AS snapshots
       JOIN realm_price_samples AS samples
         ON samples.region = snapshots.region AND samples.realm_id = snapshots.realm_id
      WHERE snapshots.region = ? AND snapshots.observed_at >= ?
        AND samples.observed_at = (
            SELECT MAX(previous.observed_at) FROM realm_price_samples AS previous
             WHERE previous.region = snapshots.region AND previous.realm_id = snapshots.realm_id
               AND previous.item_id = samples.item_id AND previous.variant_id = samples.variant_id
               AND previous.observed_at <= snapshots.observed_at
        )
 )
 SELECT expanded.item_id, expanded.region, expanded.realm_id, variants.variant,
        expanded.observed_at, expanded.min_price, expanded.median_price,
        expanded.max_price, expanded.listings
   FROM expanded JOIN market_variants AS variants ON variants.variant_id = expanded.variant_id
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
  ORDER BY item_id, realm_id, variant, observed_at
```

```text
MERGE (UNION ALL)
  LEFT
    SEARCH snapshots USING PRIMARY KEY (region=?)
    SEARCH samples USING PRIMARY KEY (ANY(item_id) AND region=? AND realm_id=?)
    CORRELATED SCALAR SUBQUERY 1
      SEARCH previous USING PRIMARY KEY (item_id=? AND region=? AND realm_id=? AND variant_id=? AND observed_at<?)
    SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
    USE TEMP B-TREE FOR ORDER BY
  RIGHT
    SEARCH samples USING PRIMARY KEY (ANY(item_id) AND region=?)
    CORRELATED SCALAR SUBQUERY 4
      SEARCH snapshots USING PRIMARY KEY (region=? AND realm_id=? AND observed_at=?)
    SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
    USE TEMP B-TREE FOR RIGHT PART OF ORDER BY
```

## deals, every published realm market in one region

Deals: one regional evidence row plus its purchasable realm rows per item.
`crates/storage/src/sqlite/read_model.rs`

```sql
SELECT * FROM market_rollup
 WHERE state = 'published' AND region = ?
 ORDER BY item_id, track, realm_id
```

```text
SEARCH market_rollup USING PRIMARY KEY (region=?)
```

## tooltips for a whole category

`get_many`, which replaced 1316 single reads per page (§11b).
`crates/storage/src/sqlite/cache.rs`

```sql
SELECT key, value FROM cache
 WHERE key IN (?, ?, ?) AND expires_at > ?
```

```text
SEARCH cache USING INDEX sqlite_autoindex_cache_1 (key=?)
```
