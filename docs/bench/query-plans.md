# Query plans

Recorded by `scripts/query-plans.py`. Regenerate it whenever an index,
a query or the statistics change, and say in the commit message what
moved -- CLAUDE.md §11b's rule is to check the plan, and a plan nobody
wrote down is a plan nobody can compare against.

Fixture: `data/bench/market-realistic.db`  
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
SEARCH price_samples USING PRIMARY KEY (ANY(item_id) AND region=? AND observed_at>? AND observed_at<?)
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
SELECT item_id, region, realm_id, variant, MAX(observed_at) AS observed_at,
       min_price, median_price, max_price, listings
  FROM realm_price_samples
 WHERE region = ?
 GROUP BY item_id, realm_id, variant
```

```text
SEARCH realm_price_samples USING PRIMARY KEY (ANY(item_id) AND region=?)
```

## per-realm latest, one realm

the same pages once a realm is chosen.  
`crates/storage/src/sqlite/realm_prices.rs`

```sql
SELECT item_id, region, realm_id, variant, MAX(observed_at) AS observed_at,
       min_price, median_price, max_price, listings
  FROM realm_price_samples
 WHERE region = ? AND realm_id = ?
 GROUP BY item_id, realm_id, variant
```

```text
SEARCH realm_price_samples USING PRIMARY KEY (ANY(item_id) AND region=? AND realm_id=?)
```

## per-realm history, one item across a region

the BoE analysis page: one track on every realm of a region.  
`crates/storage/src/sqlite/realm_prices.rs`

```sql
SELECT item_id, region, realm_id, variant, observed_at,
       min_price, median_price, max_price, listings
  FROM realm_price_samples
 WHERE item_id = ? AND region = ? AND observed_at >= ?
 ORDER BY observed_at
```

```text
SEARCH realm_price_samples USING PRIMARY KEY (item_id=? AND region=?)
USE TEMP B-TREE FOR ORDER BY
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
