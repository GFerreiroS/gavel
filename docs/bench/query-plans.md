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
SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
       samples.observed_at, samples.min_price, samples.median_price,
       samples.max_price, samples.listings
  FROM realm_price_samples AS samples
  JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
 WHERE samples.item_id = ? AND samples.region = ? AND samples.observed_at >= ?
 ORDER BY samples.observed_at
```

```text
SEARCH samples USING PRIMARY KEY (item_id=? AND region=?)
SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
USE TEMP B-TREE FOR ORDER BY
```

## per-realm history, one item on one realm

the single-realm full history view.
`crates/storage/src/sqlite/realm_prices.rs`

```sql
SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
        samples.observed_at, samples.min_price, samples.median_price,
        samples.max_price, samples.listings
   FROM realm_price_samples AS samples
   JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
  WHERE samples.item_id = ? AND samples.region = ?
        AND samples.realm_id = ? AND samples.observed_at >= ?
  ORDER BY samples.observed_at
```

```text
SEARCH samples USING INDEX idx_realm_prices_item (item_id=? AND region=? AND realm_id=? AND observed_at>?)
SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
```

## per-realm window, whole region

the background materialiser reading a window of history.
`crates/storage/src/sqlite/realm_prices.rs`

```sql
SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
        samples.observed_at, samples.min_price, samples.median_price,
        samples.max_price, samples.listings
   FROM realm_price_samples AS samples
   JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
  WHERE samples.region = ? AND samples.observed_at >= ?
  ORDER BY samples.item_id, samples.realm_id, samples.variant_id, samples.observed_at
```

```text
SEARCH samples USING PRIMARY KEY (ANY(item_id) AND region=?)
SEARCH variants USING INTEGER PRIMARY KEY (rowid=?)
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
