-- Indices shaped like the "latest price per market" window, not like the
-- filter in front of it.
--
-- Both hot queries are `ROW_NUMBER() OVER (PARTITION BY <market> ORDER BY
-- observed_at DESC)` over one region. The existing indices lead with
-- `(region, observed_at)`, which serves the WHERE and leaves SQLite to sort
-- every row in the region into partition order -- `USE TEMP B-TREE FOR ORDER
-- BY` on 162k rows, on a page a visitor is waiting for.
--
-- Leading with the partition columns and ending in `observed_at DESC` lets the
-- window walk the index in the order it already wants. The old indices stay:
-- `idx_prices_item_time` still serves a single item's history, which is what
-- an item page reads.
CREATE INDEX IF NOT EXISTS idx_realm_prices_window
    ON realm_price_samples(region, item_id, realm_id, variant, observed_at DESC);

CREATE INDEX IF NOT EXISTS idx_prices_window
    ON price_samples(region, item_id, observed_at DESC);
